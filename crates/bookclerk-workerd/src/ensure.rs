//! Download / refresh the pinned Cloudflare `workerd` binary.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::pin::{
    binary_name, download_url, host_asset, WORKERD_RELEASE_TAG, WORKERD_VERSION_STAMP,
};

/// Join a single path component under `root` after canonicalize + `starts_with`.
///
/// `edition..2` is a normal filename, not a parent component.
fn join_component_under(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.contains('\0')
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
    {
        bail!("refusing unsafe path component: {name}");
    }
    let root_norm = bookclerk_sandbox::canonicalize(root)
        .with_context(|| format!("canonicalize root {}", root.display()))?;
    let out = root_norm.join(name);
    if !out.starts_with(&root_norm) {
        bail!(
            "path {} escapes root {}",
            out.display(),
            root_norm.display()
        );
    }
    Ok(out)
}

/// Refuse a managed-cache leaf that exists as a symlink (dangling or otherwise).
fn refuse_managed_leaf_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("refusing managed cache symlink: {}", path.display());
        }
        Ok(_) | Err(_) => Ok(()),
    }
}

/// True when `path` is an existing regular file (not a symlink).
fn is_regular_file(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => meta.is_file() && !meta.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Ensure `dir` exists and return its canonical path.
///
/// The selected cache/install root is trusted (workspace `target/`, `/opt`,
/// external volumes). Children are constrained under that canonical root.
fn prepare_cache_dir(dir: &Path) -> Result<PathBuf> {
    if dir.as_os_str().is_empty() {
        bail!("refusing empty workerd cache dir");
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    bookclerk_sandbox::canonicalize(dir)
        .with_context(|| format!("canonicalize cache {}", dir.display()))
}

/// Unique same-directory staging path for an exclusive install attempt.
fn staging_install_path(dir: &Path, dest_name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nonce: u64 = rand::random();
    dir.join(format!(
        ".{dest_name}.tmp-{}-{nonce:016x}-{n}",
        std::process::id()
    ))
}

/// Replace directory entry `to` with staging file `from` without following a leaf symlink.
///
/// Unix `rename` replaces the entry. Windows cannot replace via `std::fs::rename`,
/// so an existing regular destination is unlinked first (symlinks already refused).
fn replace_cache_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        match fs::symlink_metadata(to) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "refusing to replace through a symlink destination",
                ));
            }
            Ok(_) => {
                let _ = fs::remove_file(to);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    fs::rename(from, to)
}

/// Ensure `dir/workerd` matches [`WORKERD_RELEASE_TAG`], downloading if needed.
///
/// Returns the path to the executable. Honors `BOOKCLERK_WORKERD_BIN` when set:
/// if that absolute path is a file and its sibling version stamp or `--version`
/// output matches the pin, it is returned. Otherwise ensure installs into `dir`
/// (the override path is not overwritten). A matching stamp without a binary is
/// a cache miss. `--version` uses fixed argv on the absolute path (no `PATH`
/// lookup). The selected cache directory is the trusted install root.
///
/// # Arguments
///
/// * `dir` - Directory path for this operation.
///
/// # Returns
///
/// On success, the inner `PathBuf` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn ensure_workerd(dir: &Path) -> Result<PathBuf> {
    if let Ok(override_bin) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        if !override_bin.is_empty()
            && !override_bin.contains('\0')
            && Path::new(&override_bin).is_absolute()
        {
            let path = PathBuf::from(&override_bin);
            if path.is_file() && is_current(&path)? {
                return bookclerk_sandbox::require_spawn_executable(&path)
                    .with_context(|| format!("validate workerd override {}", path.display()));
            }
        }
        // Stale/missing override: fall through to managed install in `dir`.
    }

    let dir = prepare_cache_dir(dir)?;
    let dest = join_component_under(&dir, binary_name())?;
    refuse_managed_leaf_symlink(&dest)?;
    if is_regular_file(&dest) && is_current(&dest)? {
        return bookclerk_sandbox::require_spawn_executable(&dest)
            .with_context(|| format!("validate workerd binary {}", dest.display()));
    }

    let asset = host_asset().with_context(|| {
        format!(
            "no pinned workerd asset for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let url = download_url(asset.artifact);
    eprintln!("bookclerk-workerd: fetching {url}");
    let compressed = download(&url).with_context(|| format!("download {url}"))?;
    verify_sha256(&compressed, asset.sha256_hex)?;

    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut binary = Vec::new();
    decoder
        .read_to_end(&mut binary)
        .context("gunzip workerd payload")?;

    let tmp = staging_install_path(&dir, binary_name());
    let tmp = join_component_under(
        &dir,
        tmp.file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid staging name"))?,
    )?;
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("create exclusive {}", tmp.display()))?;
        f.write_all(&binary)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;
    }
    refuse_managed_leaf_symlink(&dest)?;
    if let Err(err) = replace_cache_entry(&tmp, &dest) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| {
            format!(
                "install workerd → {} (replace {})",
                dest.display(),
                tmp.display()
            )
        });
    }

    let stamp = join_component_under(&dir, WORKERD_VERSION_STAMP)?;
    refuse_managed_leaf_symlink(&stamp)?;
    let stamp_tmp = staging_install_path(&dir, WORKERD_VERSION_STAMP);
    let stamp_tmp = join_component_under(
        &dir,
        stamp_tmp
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid stamp staging name"))?,
    )?;
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stamp_tmp)
            .with_context(|| format!("create exclusive {}", stamp_tmp.display()))?;
        f.write_all(format!("{WORKERD_RELEASE_TAG}\n").as_bytes())
            .with_context(|| format!("write {}", stamp_tmp.display()))?;
        f.sync_all().ok();
    }
    if let Err(err) = replace_cache_entry(&stamp_tmp, &stamp) {
        let _ = fs::remove_file(&stamp_tmp);
        return Err(err).with_context(|| format!("write {}", stamp.display()));
    }

    eprintln!(
        "bookclerk-workerd: installed {WORKERD_RELEASE_TAG} → {}",
        dest.display()
    );
    bookclerk_sandbox::require_spawn_executable(&dest)
        .with_context(|| format!("validate installed workerd {}", dest.display()))
}

/// Preferred install directory: beside this process, else `dir` argument from callers.
///
/// # Arguments
///
/// * `dir` - Directory path for this operation.
///
/// # Returns
///
/// `PathBuf` result.
#[must_use]
pub fn workerd_bin_path(dir: &Path) -> PathBuf {
    if let Ok(override_bin) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        if !override_bin.is_empty()
            && !override_bin.contains('\0')
            && Path::new(&override_bin).is_absolute()
        {
            return PathBuf::from(override_bin);
        }
    }
    dir.join(binary_name())
}

/// True when `bin` is a usable file and matches [`WORKERD_RELEASE_TAG`].
///
/// A sibling stamp is enough when the binary exists. A stamp without a binary
/// is not current. When no Bookclerk stamp matches, probe `--version` on the
/// absolute path with fixed argv (no `PATH` lookup). CodeQL's local threat
/// model may still taint that absolute path; the probe does not shell out.
fn is_current(bin: &Path) -> Result<bool> {
    if !bin.is_file() {
        return Ok(false);
    }
    if stamp_matches(bin)? {
        return Ok(true);
    }
    probe_version(bin)
}

/// True when the sibling `workerd.version` stamp equals the pin.
fn stamp_matches(bin: &Path) -> Result<bool> {
    let Some(parent) = bin.parent() else {
        return Ok(false);
    };
    let stamp = if parent.exists() {
        join_component_under(parent, WORKERD_VERSION_STAMP)?
    } else {
        let stamp = parent.join(WORKERD_VERSION_STAMP);
        if !stamp.starts_with(parent) {
            bail!(
                "stamp {} escapes parent {}",
                stamp.display(),
                parent.display()
            );
        }
        stamp
    };
    match fs::symlink_metadata(&stamp) {
        Ok(meta) if meta.file_type().is_symlink() => return Ok(false),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("stat {}", stamp.display()));
        }
    }
    match fs::read_to_string(&stamp) {
        Ok(text) => Ok(text.trim() == WORKERD_RELEASE_TAG),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("read {}", stamp.display())),
    }
}

/// `--version` of an absolute binary; argv is only `--version`.
fn probe_version(bin: &Path) -> Result<bool> {
    let bin = bookclerk_sandbox::require_spawn_executable(bin)
        .with_context(|| format!("validate workerd binary {}", bin.display()))?;
    let output = bookclerk_sandbox::with_fd_spawn_lock(|| {
        std::process::Command::new(&bin).arg("--version").output()
    })
    .with_context(|| format!("run {} --version", bin.display()))?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    let pin = WORKERD_RELEASE_TAG.trim_start_matches('v');
    Ok(combined.contains(WORKERD_RELEASE_TAG) || combined.contains(pin))
}

/// How many times [`download`] tries the request before it gives up.
const DOWNLOAD_ATTEMPTS: u32 = 4;

/// Wait before the first retry of [`download`]; it doubles after each retry
/// (2 s, 4 s, 8 s), so a failing download gives up after about 14 s of waiting.
const DOWNLOAD_FIRST_BACKOFF: Duration = Duration::from_secs(2);

/// Outcome of one download attempt, as [`with_retries`] sees it.
enum Attempt {
    /// The attempt read the whole body.
    Done(Vec<u8>),
    /// The attempt failed in a way a later attempt may not (a dropped
    /// connection, a timeout, a 5xx, 408 or 429 answer).
    Transient(anyhow::Error),
    /// The attempt failed in a way that retrying cannot fix (any other 4xx).
    Permanent(anyhow::Error),
}

/// Runs `attempt` until it returns [`Attempt::Done`] or [`Attempt::Permanent`],
/// or `attempts` tries have failed transiently.
///
/// # Arguments
///
/// * `attempts` - total number of tries, at least 1 (0 is treated as 1).
/// * `first_backoff` - wait after the first transient failure; doubled after
///   each further one.
/// * `sleep` - performs the wait (the real [`std::thread::sleep`] outside tests).
/// * `attempt` - performs one try.
///
/// # Returns
///
/// The bytes of the first successful attempt.
///
/// # Errors
///
/// Returns the permanent error at once, or the last transient error once
/// every attempt has failed, with the number of attempts in its context.
fn with_retries(
    attempts: u32,
    first_backoff: Duration,
    mut sleep: impl FnMut(Duration),
    mut attempt: impl FnMut() -> Attempt,
) -> Result<Vec<u8>> {
    let attempts = attempts.max(1);
    let mut backoff = first_backoff;
    let mut tried = 0;
    loop {
        tried += 1;
        match attempt() {
            Attempt::Done(body) => return Ok(body),
            Attempt::Permanent(err) => return Err(err),
            Attempt::Transient(err) if tried >= attempts => {
                return Err(err.context(format!("gave up after {tried} attempts")));
            }
            Attempt::Transient(err) => {
                eprintln!(
                    "bookclerk-workerd: attempt {tried} of {attempts} failed ({err:#}); \
                     retrying in {}s",
                    backoff.as_secs()
                );
                sleep(backoff);
                backoff = backoff.saturating_mul(2);
            }
        }
    }
}

/// Whether an HTTP status is worth retrying: server errors, 408 (request
/// timeout) and 429 (rate limited). Other 4xx answers will not change.
fn status_is_transient(status: u16) -> bool {
    status >= 500 || status == 408 || status == 429
}

/// Sorts a `ureq` error into a transient or a permanent [`Attempt`].
fn classify(url: &str, err: ureq::Error) -> Attempt {
    let transient = match &err {
        ureq::Error::StatusCode(status) => status_is_transient(*status),
        // Connection resets, timeouts, DNS and TLS hiccups on a CI runner
        // are worth another try; the pinned URL itself does not change.
        _ => true,
    };
    let err = anyhow::Error::new(err).context(format!("GET {url}"));
    if transient {
        Attempt::Transient(err)
    } else {
        Attempt::Permanent(err)
    }
}

/// One GET of `url`, reading the whole body.
///
/// `ureq` reports every non-2xx answer as [`ureq::Error::StatusCode`] (its
/// default `http_status_as_error`), so [`classify`] sees those too.
fn download_once(url: &str) -> Attempt {
    let mut response = match ureq::get(url).call() {
        Ok(response) => response,
        Err(err) => return classify(url, err),
    };
    let mut body = Vec::new();
    match response.body_mut().as_reader().read_to_end(&mut body) {
        Ok(_) => Attempt::Done(body),
        // A peer that disconnects mid-body leaves a truncated download.
        Err(err) => Attempt::Transient(anyhow::Error::new(err).context("read download body")),
    }
}

/// Downloads the pinned workerd artifact, retrying transient failures with
/// exponential backoff; fails on a permanent non-2xx answer or once every
/// attempt has failed.
fn download(url: &str) -> Result<Vec<u8>> {
    with_retries(
        DOWNLOAD_ATTEMPTS,
        DOWNLOAD_FIRST_BACKOFF,
        std::thread::sleep,
        || download_once(url),
    )
}

/// Rejects the download when the SHA-256 hex digest does not match the pin.
fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let got = hex::encode(hasher.finalize());
    if got != expected_hex {
        bail!("workerd download sha256 mismatch: got {got}, expected {expected_hex}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves one canned HTTP answer on a loopback port and returns its URL.
    fn serve_once(answer: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(answer);
        });
        format!("http://{addr}/workerd.gz")
    }

    #[test]
    fn download_once_reads_a_whole_body() {
        let url =
            serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        assert!(matches!(download_once(&url), Attempt::Done(body) if body == b"ok"));
    }

    #[test]
    fn download_once_retries_a_server_error() {
        let url = serve_once(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(matches!(download_once(&url), Attempt::Transient(_)));
    }

    #[test]
    fn download_once_does_not_retry_not_found() {
        let url =
            serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert!(matches!(download_once(&url), Attempt::Permanent(_)));
    }

    #[test]
    fn download_once_retries_a_truncated_body() {
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial",
        );
        assert!(matches!(download_once(&url), Attempt::Transient(_)));
    }

    #[test]
    fn with_retries_returns_the_first_success_after_transient_failures() {
        let mut waits = Vec::new();
        let mut calls = 0;
        let got = with_retries(
            4,
            Duration::from_secs(2),
            |d| waits.push(d),
            || {
                calls += 1;
                if calls < 3 {
                    Attempt::Transient(anyhow::anyhow!("peer disconnected"))
                } else {
                    Attempt::Done(b"ok".to_vec())
                }
            },
        )
        .unwrap();
        assert_eq!(got, b"ok");
        assert_eq!(calls, 3);
        assert_eq!(waits, [Duration::from_secs(2), Duration::from_secs(4)]);
    }

    #[test]
    fn with_retries_gives_up_after_the_last_attempt() {
        let mut waits = Vec::new();
        let mut calls = 0;
        let err = with_retries(
            4,
            Duration::from_secs(2),
            |d| waits.push(d),
            || {
                calls += 1;
                Attempt::Transient(anyhow::anyhow!("peer disconnected"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 4);
        assert_eq!(
            waits,
            [
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8)
            ]
        );
        let text = format!("{err:#}");
        assert!(text.contains("gave up after 4 attempts"), "{text}");
        assert!(text.contains("peer disconnected"), "{text}");
    }

    #[test]
    fn with_retries_stops_at_a_permanent_failure() {
        let mut waits = Vec::new();
        let mut calls = 0;
        let err = with_retries(
            4,
            Duration::from_secs(2),
            |d| waits.push(d),
            || {
                calls += 1;
                Attempt::Permanent(anyhow::anyhow!("404"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(waits.is_empty());
        assert_eq!(err.to_string(), "404");
    }

    #[test]
    fn with_retries_tries_once_when_attempts_is_zero() {
        let mut calls = 0;
        let err = with_retries(
            0,
            Duration::from_secs(2),
            |_| panic!("no wait expected"),
            || {
                calls += 1;
                Attempt::Transient(anyhow::anyhow!("reset"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(format!("{err:#}").contains("gave up after 1 attempts"));
    }

    #[test]
    fn only_server_timeout_and_rate_limit_statuses_are_transient() {
        for status in [500, 502, 503, 504, 408, 429] {
            assert!(status_is_transient(status), "{status}");
        }
        for status in [400, 401, 403, 404, 410] {
            assert!(!status_is_transient(status), "{status}");
        }
    }

    #[test]
    fn classify_sorts_ureq_errors() {
        assert!(matches!(
            classify("u", ureq::Error::StatusCode(503)),
            Attempt::Transient(_)
        ));
        assert!(matches!(
            classify("u", ureq::Error::StatusCode(404)),
            Attempt::Permanent(_)
        ));
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        match classify("https://example.invalid/w.gz", ureq::Error::Io(io)) {
            Attempt::Transient(err) => {
                assert!(err.to_string().contains("GET https://example.invalid/w.gz"));
            }
            Attempt::Done(_) | Attempt::Permanent(_) => panic!("io error should be transient"),
        }
    }

    #[test]
    fn host_asset_defined_for_ci_linux() {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert!(host_asset().is_some());
        }
    }

    #[test]
    fn prepare_cache_dir_accepts_path_outside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let got = prepare_cache_dir(tmp.path()).unwrap();
        assert_eq!(got, fs::canonicalize(tmp.path()).unwrap());
    }

    #[test]
    fn stamp_without_binary_is_not_current() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(WORKERD_VERSION_STAMP),
            format!("{WORKERD_RELEASE_TAG}\n"),
        )
        .unwrap();
        let bin = tmp.path().join(binary_name());
        assert!(!is_current(&bin).unwrap());
    }

    #[test]
    fn stamp_and_file_is_current_without_executing() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(binary_name());
        fs::write(&bin, b"not-a-real-binary").unwrap();
        fs::write(
            tmp.path().join(WORKERD_VERSION_STAMP),
            format!("{WORKERD_RELEASE_TAG}\n"),
        )
        .unwrap();
        assert!(is_current(&bin).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_accepts_absolute_binary_without_stamp() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("fake-workerd");
        fs::write(
            &bin,
            format!("#!/bin/sh\nprintf '%s\\n' '{WORKERD_RELEASE_TAG}'\n"),
        )
        .unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
        assert!(is_current(&bin).unwrap());
    }
}
