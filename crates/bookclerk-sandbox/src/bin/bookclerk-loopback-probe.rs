//! Same-AppContainer loopback probe (experiment E3).
//!
//! Binds `127.0.0.1:0`, spawns a child of this binary that connects back, and
//! completes a four-byte echo. Used by `tests/windows_loopback.rs`.

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("bookclerk-loopback-probe: {err}");
            1
        }
    };
    std::process::exit(code);
}

/// Dispatch bind/echo or the child `--connect` role.
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--connect") => {
            let host = args.next().ok_or("missing host")?;
            let port: u16 = args
                .next()
                .ok_or("missing port")?
                .parse()
                .map_err(|err| format!("port: {err}"))?;
            connect_echo(&host, port)
        }
        Some(other) => Err(format!("unknown argument: {other}")),
        None => bind_and_echo(),
    }
}

/// Bind loopback, spawn a connecting child, and echo four bytes.
fn bind_and_echo() -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::time::Duration;

    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|err| format!("bind: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?
        .port();
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let mut child = Command::new(exe)
        .arg("--connect")
        .arg("127.0.0.1")
        .arg(port.to_string())
        .spawn()
        .map_err(|err| format!("spawn child: {err}"))?;
    let (mut stream, _) = listener.accept().map_err(|err| format!("accept: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("timeout: {err}"))?;
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .map_err(|err| format!("read: {err}"))?;
    stream
        .write_all(&buf)
        .map_err(|err| format!("write: {err}"))?;
    let status = child.wait().map_err(|err| format!("wait: {err}"))?;
    if !status.success() {
        return Err(format!("child failed: {status:?}"));
    }
    println!(
        "{}",
        serde_json::json!({ "loopback_ok": true, "port": port })
    );
    Ok(())
}

/// Connect to `host:port` and complete a four-byte echo.
fn connect_echo(host: &str, port: u16) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect((host, port)).map_err(|err| format!("connect: {err}"))?;
    stream
        .write_all(b"ping")
        .map_err(|err| format!("write: {err}"))?;
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .map_err(|err| format!("read: {err}"))?;
    if &buf != b"ping" {
        return Err(format!("echo mismatch: {buf:?}"));
    }
    Ok(())
}
