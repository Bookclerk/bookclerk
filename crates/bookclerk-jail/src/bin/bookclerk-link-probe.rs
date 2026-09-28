//! Inherited-link probe for sibling-sandbox experiments E2/E4.
//!
//! Echoes bytes over an inherited fd or `handle:` and attempts a TCP connect
//! that must fail under `NetPolicy::Deny`.

#![cfg_attr(any(unix, windows), allow(unsafe_code))]

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::Duration;

use bookclerk_sandbox::{LinkSpec, SOCKET_PROXY_ENV};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bookclerk-link-probe: {err}");
            ExitCode::from(1)
        }
    }
}

/// Parse flags and run the requested echo / TCP / fd probes.
fn run() -> Result<(), String> {
    let mut echo_fd: Option<i32> = None;
    let mut echo_handle = false;
    let mut send_ping = false;
    let mut deny_tcp: Option<(String, u16)> = None;
    let mut closed_fd: Option<i32> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--echo-fd" => {
                echo_fd = Some(
                    args.next()
                        .ok_or("--echo-fd needs a number")?
                        .parse()
                        .map_err(|err| format!("bad --echo-fd: {err}"))?,
                );
            }
            "--echo-handle" => echo_handle = true,
            "--send-ping" => send_ping = true,
            "--deny-tcp" => {
                let host = args.next().ok_or("--deny-tcp needs host")?;
                let port: u16 = args
                    .next()
                    .ok_or("--deny-tcp needs port")?
                    .parse()
                    .map_err(|err| format!("bad --deny-tcp port: {err}"))?;
                deny_tcp = Some((host, port));
            }
            "--closed-fd" => {
                closed_fd = Some(
                    args.next()
                        .ok_or("--closed-fd needs a number")?
                        .parse()
                        .map_err(|err| format!("bad --closed-fd: {err}"))?,
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if let Some(fd) = echo_fd {
        echo_raw_fd(fd)?;
    }
    if echo_handle {
        echo_inherited_handle()?;
    }
    if send_ping {
        send_ping_on_handle()?;
    }
    if let Some(fd) = closed_fd {
        assert_fd_closed(fd)?;
    }

    let mut tcp_denied = None;
    if let Some((host, port)) = deny_tcp {
        tcp_denied = Some(tcp_connect_denied(&host, port)?);
    }

    println!(
        "{}",
        serde_json::json!({
            "echoed": echo_fd.is_some() || echo_handle,
            "sent_ping": send_ping,
            "closed_fd": closed_fd,
            "tcp_denied": tcp_denied,
        })
    );
    Ok(())
}

/// Read four bytes from `fd` and write them back.
fn echo_raw_fd(fd: i32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut buf = [0u8; 4];
        file.read_exact(&mut buf)
            .map_err(|err| format!("read fd {fd}: {err}"))?;
        file.write_all(&buf)
            .map_err(|err| format!("write fd {fd}: {err}"))?;
        file.flush()
            .map_err(|err| format!("flush fd {fd}: {err}"))?;
        std::mem::forget(file);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err("--echo-fd is Unix-only".into())
    }
}

/// Echo four bytes over `BOOKCLERK_SOCKET_PROXY=handle:<n>`.
fn echo_inherited_handle() -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{FromRawHandle, RawHandle};

        let spec = std::env::var(SOCKET_PROXY_ENV)
            .map_err(|_| format!("{SOCKET_PROXY_ENV} is not set"))?;
        let LinkSpec::Handle(value) = LinkSpec::parse(&spec).map_err(|err| err.to_string())? else {
            return Err(format!("{SOCKET_PROXY_ENV} must be handle:<n>, got {spec}"));
        };
        let mut file = unsafe { std::fs::File::from_raw_handle(value as usize as RawHandle) };
        let mut buf = [0u8; 4];
        file.read_exact(&mut buf)
            .map_err(|err| format!("read handle: {err}"))?;
        file.write_all(&buf)
            .map_err(|err| format!("write handle: {err}"))?;
        file.flush().map_err(|err| format!("flush handle: {err}"))?;
        std::mem::forget(file);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = SOCKET_PROXY_ENV;
        let _ = LinkSpec::parse;
        Err("--echo-handle is Windows-only".into())
    }
}

/// Write `ping` on the inherited handle and expect the same four bytes back.
fn send_ping_on_handle() -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{FromRawHandle, RawHandle};

        let spec = std::env::var(SOCKET_PROXY_ENV)
            .map_err(|_| format!("{SOCKET_PROXY_ENV} is not set"))?;
        let LinkSpec::Handle(value) = LinkSpec::parse(&spec).map_err(|err| err.to_string())? else {
            return Err(format!("{SOCKET_PROXY_ENV} must be handle:<n>, got {spec}"));
        };
        let mut file = unsafe { std::fs::File::from_raw_handle(value as usize as RawHandle) };
        file.write_all(b"ping")
            .map_err(|err| format!("send-ping write: {err}"))?;
        file.flush()
            .map_err(|err| format!("send-ping flush: {err}"))?;
        let mut buf = [0u8; 4];
        file.read_exact(&mut buf)
            .map_err(|err| format!("send-ping read: {err}"))?;
        if &buf != b"ping" {
            return Err(format!("send-ping echo mismatch: {buf:?}"));
        }
        std::mem::forget(file);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err("--send-ping is Windows-only".into())
    }
}

/// Fail if `fd` is still open in this process.
fn assert_fd_closed(fd: i32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            return Err(format!("fd {fd} was still open"));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err("--closed-fd is Unix-only".into())
    }
}

/// Attempt an ambient TCP connect that must fail under `NetPolicy::Deny`.
fn tcp_connect_denied(host: &str, port: u16) -> Result<bool, String> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|err| format!("resolve {host}:{port}: {err}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}:{port}"))?;
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => Err(format!(
            "ambient TCP to {host}:{port} succeeded under NetPolicy::Deny"
        )),
        Err(_) => Ok(true),
    }
}
