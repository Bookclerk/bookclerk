//! Guest-local Unix sockets that splice into the workerd TCP gateway.
//!
//! sqlx/SeaORM only dial TCP or `{dir}/.s.PGSQL.{port}`. When
//! [`bookclerk_plugin_sdk::SOCKET_PROXY_ENV`] is set, every engine connect is
//! rewritten onto a short Unix listener in this process; accept loops call
//! [`bookclerk_plugin_sdk::connect_socket`] so ambient `AF_INET` is unused.

#![allow(clippy::missing_docs_in_private_items)]

use sea_orm::DbErr;

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::OnceLock;

#[cfg(unix)]
use crate::postgres::postgres_tcp_target;

/// Rewrites `url` so sqlx dials a guest-local Unix socket that is spliced
/// through the Bookclerk socket proxy.
///
/// When `BOOKCLERK_SOCKET_PROXY` is unset (plugin crate unit tests), the
/// original URL is returned unchanged unless nested Deny is in force. When
/// [`bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV`] is `1` without a proxy,
/// this fails closed — nested `NetPolicy::Deny` forbids ambient TCP. When
/// the proxy is set on a non-Unix host, this also fails closed because sqlx
/// has no Windows Unix-socket path to splice through the named-pipe
/// SOCKET_PROXY.
///
/// # Errors
///
/// Returns when nested Deny is set without SOCKET_PROXY, the URL is not a
/// TCP Postgres target, the local listener cannot be bound, the rewritten
/// URL cannot be produced, or the platform cannot mediate through SOCKET_PROXY.
pub async fn mediated_connect_url(url: &str) -> Result<String, DbErr> {
    if std::env::var_os(bookclerk_plugin_sdk::SOCKET_PROXY_ENV).is_none() {
        if bookclerk_plugin_sdk::nested_native_jail_requested() {
            return Err(DbErr::Custom(format!(
                "{} is required when {}=1; nested Deny forbids ambient TCP",
                bookclerk_plugin_sdk::SOCKET_PROXY_ENV,
                bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV,
            )));
        }
        return Ok(url.to_string());
    }
    #[cfg(not(unix))]
    {
        let _ = url;
        Err(DbErr::Custom(
            "postgres guest cannot splice sqlx through SOCKET_PROXY on this platform; nested Deny forbids ambient TCP"
                .to_string(),
        ))
    }
    #[cfg(unix)]
    {
        let (host, port) = postgres_tcp_target(url).ok_or_else(|| {
            DbErr::Custom(
                "postgres guest cannot dial a Unix-socket URL through the workerd socket proxy"
                    .into(),
            )
        })?;
        let socket_dir = ensure_forwarder(host, port).await?;
        postgres_url_with_unix_host(url, &socket_dir)
    }
}

/// Sets (or replaces) the libpq `host=` query so sqlx uses `socket_dir`.
///
/// sqlx `sslmode=prefer` (the default) sends `SSLRequest` even on a Unix
/// socket. Through the splice that becomes TLS-to-origin. Docker and many
/// operator servers answer `S` then present a self-signed cert; Prefer does
/// not fall back after `S`, so the handshake RST looks like a connection
/// error. Keep `require` / `verify-ca` / `verify-full` for operators who
/// asked for origin TLS; otherwise force `disable` on this local hop.
///
/// # Errors
///
/// Returns when `url` is not a valid Postgres URL.
#[cfg(any(unix, test))]
pub fn postgres_url_with_unix_host(url: &str, socket_dir: &str) -> Result<String, DbErr> {
    let mut parsed =
        url::Url::parse(url).map_err(|err| DbErr::Custom(format!("postgres URL: {err}")))?;
    let query: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let keep_origin_tls = query.iter().any(|(key, value)| {
        (key == "sslmode" || key == "ssl-mode")
            && matches!(value.as_str(), "require" | "verify-ca" | "verify-full")
    });
    parsed.set_query(None);
    {
        let mut pairs = parsed.query_pairs_mut();
        for (key, value) in &query {
            if key == "host" || key == "hostaddr" {
                continue;
            }
            if (key == "sslmode" || key == "ssl-mode") && !keep_origin_tls {
                continue;
            }
            pairs.append_pair(key, value);
        }
        pairs.append_pair("host", socket_dir);
        if !keep_origin_tls {
            pairs.append_pair("sslmode", "disable");
        }
    }
    Ok(parsed.into())
}

#[cfg(unix)]
struct Mediator {
    /// Directory fd path (`/proc/self/fd/N` on Linux) or short `/tmp` dir.
    sqlx_host: String,
    /// Keeps `/proc/self/fd/N` valid for sqlx `connect`.
    _dir_fd: std::fs::File,
    started: HashSet<(String, u16)>,
}

#[cfg(unix)]
fn mediator() -> &'static tokio::sync::Mutex<Option<Mediator>> {
    static CELL: OnceLock<tokio::sync::Mutex<Option<Mediator>>> = OnceLock::new();
    CELL.get_or_init(|| tokio::sync::Mutex::new(None))
}

#[cfg(unix)]
async fn ensure_forwarder(host: String, port: u16) -> Result<String, DbErr> {
    let mut slot = mediator().lock().await;
    if slot.is_none() {
        *slot = Some(open_mediator()?);
    }
    let med = slot
        .as_mut()
        .ok_or_else(|| DbErr::Custom("postgres socket mediator failed to initialize".into()))?;
    if med.started.insert((host.clone(), port)) {
        spawn_accept_loop(&med.sqlx_host, host, port)?;
    }
    Ok(med.sqlx_host.clone())
}

#[cfg(unix)]
fn open_mediator() -> Result<Mediator, DbErr> {
    let dir = mediator_dir();
    std::fs::create_dir_all(&dir).map_err(|err| {
        DbErr::Custom(format!(
            "postgres socket mediator mkdir {}: {err}",
            dir.display()
        ))
    })?;
    let dir_fd = std::fs::File::open(&dir).map_err(|err| {
        DbErr::Custom(format!(
            "postgres socket mediator open {}: {err}",
            dir.display()
        ))
    })?;
    let sqlx_host = sqlx_host_for(&dir, &dir_fd);
    Ok(Mediator {
        sqlx_host,
        _dir_fd: dir_fd,
        started: HashSet::new(),
    })
}

#[cfg(unix)]
fn mediator_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("TMPDIR")
            .or_else(|| std::env::var_os("TMP"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join(format!("pg-uds-{}", std::process::id()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from(format!("/tmp/bc-pg-{}", std::process::id()))
    }
}

#[cfg(unix)]
fn sqlx_host_for(dir: &Path, dir_fd: &std::fs::File) -> String {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let _ = dir;
        format!("/proc/self/fd/{}", dir_fd.as_raw_fd())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir_fd;
        dir.to_string_lossy().into_owned()
    }
}

#[cfg(unix)]
fn spawn_accept_loop(sqlx_host: &str, host: String, port: u16) -> Result<(), DbErr> {
    let name = format!(".s.PGSQL.{port}");
    let bind_path = PathBuf::from(sqlx_host).join(&name);
    let std_listener = bind_sqlx_socket(&bind_path).map_err(|err| {
        DbErr::Custom(format!(
            "postgres socket mediator bind {}: {err}",
            bind_path.display()
        ))
    })?;
    std_listener
        .set_nonblocking(true)
        .map_err(|err| DbErr::Custom(format!("postgres socket mediator nonblocking: {err}")))?;
    let listener = tokio::net::UnixListener::from_std(std_listener)
        .map_err(|err| DbErr::Custom(format!("postgres socket mediator tokio: {err}")))?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((local, _)) => {
                    let dest_host = host.clone();
                    tokio::spawn(async move {
                        if let Err(err) = splice_to_proxy(local, dest_host, port).await {
                            tracing::warn!(
                                error = %err,
                                "postgres socket mediator could not splice through the socket proxy"
                            );
                            eprintln!("postgres socket mediator: {err}");
                        }
                    });
                }
                Err(err) => {
                    tracing::debug!(error = %err, "postgres socket mediator accept failed");
                    break;
                }
            }
        }
    });
    Ok(())
}

#[cfg(unix)]
fn bind_sqlx_socket(path: &Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(err)
            if err.kind() == std::io::ErrorKind::InvalidInput
                || path.as_os_str().len() >= sun_path_capacity() =>
        {
            bind_sqlx_socket_short(path)
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn sun_path_capacity() -> usize {
    if cfg!(target_os = "macos") {
        104
    } else {
        108
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn bind_sqlx_socket_short(path: &Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixListener;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unix socket path has no parent directory",
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unix socket path has no file name",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let dir = std::fs::File::open(parent)?;
    let proc_path = format!(
        "/proc/self/fd/{}/{}",
        dir.as_raw_fd(),
        Path::new(name).display()
    );
    let listener = UnixListener::bind(&proc_path)?;
    // `dir` must outlive bind only; the inode lives in `parent`. Keep the
    // mediator dir fd (separate File) for sqlx connect.
    drop(dir);
    Ok(listener)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn bind_sqlx_socket_short(path: &Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;
    static CHDIR_BIND: Mutex<()> = Mutex::new(());
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unix socket path has no parent directory",
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unix socket path has no file name",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let _guard = CHDIR_BIND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cwd = std::env::current_dir()?;
    std::env::set_current_dir(parent)?;
    let result = UnixListener::bind(name);
    let restore = std::env::set_current_dir(&cwd);
    match (result, restore) {
        (Ok(listener), Ok(())) => Ok(listener),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

#[cfg(unix)]
async fn splice_to_proxy(
    local: tokio::net::UnixStream,
    host: String,
    port: u16,
) -> Result<(), bookclerk_plugin_sdk::SdkError> {
    let remote = bookclerk_plugin_sdk::connect_socket(
        bookclerk_plugin_sdk::SocketAddress {
            hostname: host,
            port,
        },
        bookclerk_plugin_sdk::ConnectOptions::default(),
    )
    .await?;
    let (mut lr, mut lw) = local.into_split();
    let (mut rr, mut rw) = remote.into_split();
    let _ = tokio::join!(
        tokio::io::copy(&mut lr, &mut rw),
        tokio::io::copy(&mut rr, &mut lw),
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::await_holding_lock)]
mod tests {
    use super::*;

    /// `SOCKET_PROXY_ENV` is process-wide; tests that read or mutate it must not overlap.
    static SOCKET_PROXY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn unix_host_query_overrides_tcp_authority() {
        let out = postgres_url_with_unix_host(
            "postgres://postgres:postgres@localhost:5432/bookclerk?sslmode=prefer",
            "/proc/self/fd/7",
        )
        .expect("rewrite");
        let parsed = url::Url::parse(&out).expect("url");
        assert_eq!(parsed.username(), "postgres");
        assert_eq!(parsed.password(), Some("postgres"));
        assert_eq!(parsed.path(), "/bookclerk");
        let host = parsed
            .query_pairs()
            .find(|(k, _)| k == "host")
            .map(|(_, v)| v.into_owned());
        assert_eq!(host.as_deref(), Some("/proc/self/fd/7"));
        assert!(parsed
            .query_pairs()
            .any(|(k, v)| k == "sslmode" && v == "disable"));
    }

    #[test]
    fn unix_host_keeps_require_sslmode() {
        let out = postgres_url_with_unix_host(
            "postgres://u@db.example.com:5432/db?sslmode=require",
            "/tmp/bc-pg",
        )
        .expect("rewrite");
        let parsed = url::Url::parse(&out).expect("url");
        assert!(parsed
            .query_pairs()
            .any(|(k, v)| k == "sslmode" && v == "require"));
    }

    #[test]
    fn unix_host_replaces_existing_host_query() {
        let out = postgres_url_with_unix_host("postgres://h/db?host=db.example.com", "/tmp/bc-pg")
            .expect("rewrite");
        let parsed = url::Url::parse(&out).expect("url");
        let hosts: Vec<_> = parsed
            .query_pairs()
            .filter(|(k, _)| k == "host")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(hosts, vec!["/tmp/bc-pg".to_string()]);
    }

    #[tokio::test]
    async fn mediate_is_noop_without_socket_proxy() {
        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(bookclerk_plugin_sdk::SOCKET_PROXY_ENV);
        let previous_jail = std::env::var_os(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV);
        std::env::remove_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV);
        std::env::remove_var(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV);
        let url = "postgres://postgres:postgres@localhost:5432/postgres";
        let out = mediated_connect_url(url).await.expect("noop");
        assert_eq!(out, url);
        match previous {
            Some(value) => std::env::set_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV, value),
            None => std::env::remove_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV),
        }
        match previous_jail {
            Some(value) => std::env::set_var(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV, value),
            None => std::env::remove_var(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV),
        }
    }

    #[tokio::test]
    async fn nested_jail_without_socket_proxy_fails_closed() {
        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(bookclerk_plugin_sdk::SOCKET_PROXY_ENV);
        let previous_jail = std::env::var_os(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV);
        std::env::remove_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV);
        std::env::set_var(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV, "1");
        let err = mediated_connect_url("postgres://postgres@localhost:5432/postgres")
            .await
            .expect_err("nested Deny must not use ambient TCP");
        let msg = err.to_string();
        assert!(msg.contains("SOCKET_PROXY"), "{msg}");
        match previous {
            Some(value) => std::env::set_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV, value),
            None => std::env::remove_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV),
        }
        match previous_jail {
            Some(value) => std::env::set_var(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV, value),
            None => std::env::remove_var(bookclerk_plugin_sdk::NESTED_NATIVE_JAIL_ENV),
        }
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn mediate_fails_closed_when_socket_proxy_is_set() {
        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(bookclerk_plugin_sdk::SOCKET_PROXY_ENV);
        std::env::set_var(
            bookclerk_plugin_sdk::SOCKET_PROXY_ENV,
            r"\\.\pipe\bc-s-test",
        );
        let err = mediated_connect_url("postgres://postgres@localhost:5432/postgres")
            .await
            .expect_err("windows proxy must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot splice sqlx through SOCKET_PROXY"),
            "{msg}"
        );
        match previous {
            Some(value) => std::env::set_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV, value),
            None => std::env::remove_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mediator_splices_through_socket_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dir = std::env::temp_dir().join(format!(
            "pg-med-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let proxy_path = dir.join("proxy.sock");
        let listener = tokio::net::UnixListener::bind(&proxy_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 256];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(
                req.contains("CONNECT localhost:5432"),
                "proxy request: {req}"
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            stream.write_all(b"SQL").await.unwrap();
        });
        std::env::set_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV, &proxy_path);
        let rewritten = mediated_connect_url("postgres://postgres@localhost:5432/postgres")
            .await
            .expect("mediate");
        let parsed = url::Url::parse(&rewritten).expect("rewritten url");
        let sock_dir = parsed
            .query_pairs()
            .find(|(key, _)| key == "host")
            .map(|(_, value)| value.into_owned())
            .expect("host query");
        assert!(
            sock_dir.starts_with('/'),
            "sqlx host must be a Unix directory: {sock_dir}"
        );
        let sock = format!("{sock_dir}/.s.PGSQL.5432");
        let mut client = tokio::net::UnixStream::connect(&sock)
            .await
            .unwrap_or_else(|err| panic!("connect {sock}: {err}"));
        let mut buf = [0_u8; 8];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"SQL");
        server.await.unwrap();
        std::env::remove_var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
