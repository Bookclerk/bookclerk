//! `bookclerk clusters join --url <library>`: join with a sign-in at the
//! library (RFC 8252 for a native app: the authorization code comes back to
//! a loopback listener of this process).
//!
//! 1. Read the library's `GET /api/node/v1/info` under the transport
//!    policy; a self-signed library's fingerprint must be confirmed (typed
//!    `yes`, or `--fingerprint`).
//! 2. Listen on `127.0.0.1:<random port>`, print the authorize URL, and
//!    open a browser when one is present.
//! 3. Take the first `GET /join/callback` whose `state` matches, redeem the
//!    code with the PKCE verifier only this process holds, and record the
//!    link. A running daemon starts the link's client within a few seconds.
//!
//! The verifier, `state`, code and node keys never go on argv or into a
//! log.

use std::time::{Duration, Instant};

use bookclerk_node_client::{
    authorize_branch, inspect_library, redeem_branch_code, BranchRedeem, InspectedLibrary,
};
use bookclerk_node_proto::JOIN_CALLBACK_PATH;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

/// How long the CLI waits for the browser to come back.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Largest request head the loopback listener reads.
const MAX_CALLBACK_HEAD_BYTES: usize = 8 * 1024;

/// What the browser brought back.
#[derive(PartialEq, Eq)]
pub struct Callback {
    /// The authorization code.
    pub code: Zeroizing<String>,
    /// The library's `error`, when it denied.
    pub error: Option<String>,
}

impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callback")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// Constant-time equality.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// The page the browser sees after the callback.
fn page(status: &str, text: &str) -> Vec<u8> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Bookclerk</title>\
         <body style=\"font-family:sans-serif;margin:3rem\"><p>{text}</p></body>"
    );
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Reads one request head from `stream` (bounded).
async fn read_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(buf).ok();
        }
        if buf.len() > MAX_CALLBACK_HEAD_BYTES {
            return None;
        }
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// Waits on `listener` for `GET /join/callback?code=...&state=<state>`
/// until `deadline`. Requests for other paths, or with another `state`,
/// are answered and ignored.
///
/// # Errors
///
/// Returns an error when the deadline passes or the listener fails.
pub async fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
    deadline: Instant,
) -> anyhow::Result<Callback> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("the sign-in did not come back within 10 minutes; run join again");
        }
        let Ok(accepted) = tokio::time::timeout(remaining, listener.accept()).await else {
            anyhow::bail!("the sign-in did not come back within 10 minutes; run join again");
        };
        let (mut stream, _) = accepted?;
        let Some(head) = read_head(&mut stream).await else {
            continue;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| {
                let mut parts = line.split_whitespace();
                (parts.next() == Some("GET")).then(|| parts.next())?
            })
            .unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != JOIN_CALLBACK_PATH {
            let _ = stream.write_all(&page("404 Not Found", "Not found.")).await;
            continue;
        }
        let mut code = None;
        let mut came_back = None;
        let mut error = None;
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match name.as_ref() {
                "code" => code = Some(Zeroizing::new(value.into_owned())),
                "state" => came_back = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                _ => {}
            }
        }
        if !came_back.as_deref().is_some_and(|got| same(got, state)) {
            let _ = stream
                .write_all(&page(
                    "400 Bad Request",
                    "This sign-in is not the one the terminal is waiting for.",
                ))
                .await;
            continue;
        }
        let text = if error.is_some() {
            "The library did not let this branch join. You can close this tab."
        } else {
            "Signed in. Return to the terminal; you can close this tab."
        };
        let _ = stream.write_all(&page("200 OK", text)).await;
        let _ = stream.shutdown().await;
        return Ok(Callback {
            code: code.unwrap_or_default(),
            error,
        });
    }
}

/// Opens `url` in a browser when this session has one. Best effort; the
/// URL is printed either way. It carries no secret (the PKCE challenge
/// and the `state`, which is useless without the verifier).
fn open_browser(url: &str) {
    if std::env::var_os("BOOKCLERK_NO_BROWSER").is_some() {
        return;
    }
    let spawned = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        std::process::Command::new("xdg-open").arg(url).spawn()
    } else {
        return;
    };
    if let Ok(mut child) = spawned {
        // Reap it in the background; its exit status does not matter.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Asks the operator to confirm a self-signed library's fingerprint.
fn confirm_fingerprint(library: &InspectedLibrary, given: Option<&str>) -> anyhow::Result<()> {
    use std::io::{BufRead as _, IsTerminal as _};
    let Some(pin) = library.spki_pin.as_deref() else {
        return Ok(());
    };
    if let Some(given) = given {
        if same(given.trim(), pin) {
            return Ok(());
        }
        anyhow::bail!("the library's fingerprint is {pin}, not the one given; not joining");
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "{} uses a self-signed certificate with fingerprint {pin}; check it on the library's Settings → Branches page and pass --fingerprint",
            library.origin
        );
    }
    eprintln!(
        "{} uses a self-signed certificate.\nFingerprint: {pin}\n\
         Check that it matches the one on the library's Settings → Branches page.",
        library.origin
    );
    eprint!("Type yes if it matches: ");
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        anyhow::bail!("fingerprint not confirmed; not joining")
    }
}

/// Runs `clusters join --url`.
///
/// # Errors
///
/// Returns an error when the library cannot be read or refuses, the
/// fingerprint is not confirmed, the sign-in does not come back, or the
/// link cannot be recorded.
pub async fn join_with_sign_in(
    store: &bookclerk_library::LibraryStore,
    address: &str,
    branch_name: Option<&str>,
    fingerprint: Option<&str>,
) -> anyhow::Result<bookclerk_library::control_plane::ClusterLink> {
    let library = inspect_library(address)
        .await
        .map_err(|err| anyhow::anyhow!("could not read the library: {err}"))?;
    if !library.info.enrollment {
        anyhow::bail!("This library isn't accepting new branches");
    }
    confirm_fingerprint(&library, fingerprint)?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{JOIN_CALLBACK_PATH}");
    let branch_name = branch_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(default_branch_name, str::to_string);
    let authorization = authorize_branch(&library, &redirect_uri, &branch_name)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    eprintln!(
        "Sign in to {} to let \"{branch_name}\" join it:\n\n  {}\n",
        library.info.library_name, authorization.authorize_url
    );
    open_browser(&authorization.authorize_url);
    eprintln!("Waiting for the sign-in (10 minutes)...");
    let callback = wait_for_callback(
        &listener,
        &authorization.state,
        Instant::now() + CALLBACK_TIMEOUT,
    )
    .await?;
    drop(listener);
    if callback.error.is_some() {
        anyhow::bail!("the library's user did not let this branch join");
    }
    if callback.code.is_empty() {
        anyhow::bail!("the sign-in came back without a code");
    }
    redeem_branch_code(
        store,
        &BranchRedeem {
            origin: &library.origin,
            spki_pin: library.spki_pin.as_deref(),
            cluster_id: &library.info.cluster_id,
            code: &callback.code,
            verifier: &authorization.verifier,
            redirect_uri: &authorization.redirect_uri,
            branch_name: &branch_name,
            software_version: env!("CARGO_PKG_VERSION"),
        },
        &authorization.keys,
    )
    .await
    .map_err(|err| anyhow::anyhow!("could not join the library: {err}"))
}

/// This machine's name, as a default branch name.
fn default_branch_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().chars().take(64).collect::<String>())
        .filter(|name| bookclerk_node_proto::is_branch_name(name))
        .unwrap_or_else(|| "branch".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer).await;
        answer
    }

    #[tokio::test]
    async fn the_loopback_callback_takes_only_its_own_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = "s".repeat(43);
        let waiting = {
            let state = state.clone();
            tokio::spawn(async move {
                wait_for_callback(&listener, &state, Instant::now() + Duration::from_secs(20)).await
            })
        };
        // Another path, then another state, are answered and ignored.
        assert!(get(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let wrong = get(port, "/join/callback?code=c1&state=nope").await;
        assert!(wrong.starts_with("HTTP/1.1 400"), "{wrong}");
        let ok = get(port, &format!("/join/callback?code=the-code&state={state}")).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("Cache-Control: no-store"));
        assert!(!ok.contains("the-code"), "the code is not echoed");
        let callback = waiting.await.unwrap().unwrap();
        assert_eq!(callback.code.as_str(), "the-code");
        assert_eq!(callback.error, None);
        assert!(!format!("{callback:?}").contains("the-code"));
    }

    #[tokio::test]
    async fn a_denied_sign_in_comes_back_as_an_error_and_the_wait_ends() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiting = tokio::spawn(async move {
            wait_for_callback(&listener, "abc", Instant::now() + Duration::from_secs(20)).await
        });
        get(port, "/join/callback?error=access_denied&state=abc").await;
        let callback = waiting.await.unwrap().unwrap();
        assert_eq!(callback.error.as_deref(), Some("access_denied"));
        assert!(callback.code.is_empty());

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let err = wait_for_callback(&listener, "abc", Instant::now())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("10 minutes"));
    }
}
