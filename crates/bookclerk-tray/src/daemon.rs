//! Ensure `bookclerkd` is reachable and talk to its HTTP API.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use bookclerk_config::{
    operator_token_path, read_operator_token, read_or_create_operator_token, Config,
};

pub struct DaemonHandle {
    pub base_url: String,
    child: Option<Child>,
}

impl DaemonHandle {
    pub fn base_url(listen: &str) -> String {
        let listen = listen.trim().trim_end_matches('/');
        if listen.starts_with("http://") || listen.starts_with("https://") {
            listen.to_string()
        } else {
            format!("http://{listen}")
        }
    }

    pub fn ensure(config: &Config) -> anyhow::Result<Self> {
        let listen = config.daemon.listen.clone();
        let base_url = Self::base_url(&listen);
        let files_dir = config.paths().files_dir.clone();

        let mut child = None;
        if !daemon_reachable(&base_url) {
            let mut spawned = spawn_daemon(&files_dir, &listen)?;
            for _ in 0..50 {
                if daemon_reachable(&base_url) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !daemon_reachable(&base_url) {
                let _ = spawned.kill();
                let _ = spawned.wait();
                anyhow::bail!("bookclerkd did not become healthy at {base_url}");
            }
            child = Some(spawned);
        }

        if config.daemon.auth.enabled {
            // Only mint a token when *we* spawned bookclerkd. An already-running
            // daemon caches its token at startup; overwriting the file would
            // desync Bearer auth for scan / API calls.
            if child.is_some() {
                let _ = read_or_create_operator_token(config)?;
            } else if read_operator_token(config)?.is_none() {
                eprintln!(
                    "bookclerk-tray: attached to existing bookclerkd but no operator token \
                     found at {} (or BOOKCLERK_OPERATOR_TOKEN); scan / auto-login may fail",
                    operator_token_path(config).display()
                );
            }
            eprintln!(
                "bookclerk-tray: operator token file {}",
                operator_token_path(config).display()
            );
        }

        Ok(Self { base_url, child })
    }

    pub fn open_ui(&self) -> anyhow::Result<()> {
        open::that(&self.base_url)?;
        Ok(())
    }

    pub fn trigger_scan(&self, config: &Config) -> anyhow::Result<()> {
        let url = format!("{}/api/library/scan", self.base_url);
        let mut req = ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(10));
        if config.daemon.auth.enabled {
            let token = existing_operator_token(config)?;
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        req.send_string("{}")?;
        Ok(())
    }

    pub fn print_operator_token(&self, config: &Config) {
        if !config.daemon.auth.enabled {
            eprintln!("bookclerk-tray: operator auth is disabled");
            return;
        }
        match existing_operator_token(config) {
            Ok(token) => {
                eprintln!(
                    "bookclerk-tray: operator token (file {}):\n{token}",
                    operator_token_path(config).display()
                );
            }
            Err(err) => eprintln!("bookclerk-tray: {err}"),
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Read an existing operator token; never mint a new file (avoids desync with a
/// running daemon that already loaded its token).
fn existing_operator_token(config: &Config) -> anyhow::Result<String> {
    match read_operator_token(config)? {
        Some((token, _)) => Ok(token),
        None => anyhow::bail!(
            "no operator token at {} (or BOOKCLERK_OPERATOR_TOKEN); not creating one while \
             attaching to a running bookclerkd",
            operator_token_path(config).display()
        ),
    }
}

fn daemon_reachable(base: &str) -> bool {
    ureq::get(&format!("{base}/health"))
        .timeout(Duration::from_secs(2))
        .call()
        .is_ok()
}

fn spawn_daemon(files_dir: &Path, listen: &str) -> anyhow::Result<Child> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let candidates = [
        dir.join("bookclerkd"),
        PathBuf::from("bookclerkd"),
        PathBuf::from("target/debug/bookclerkd"),
        PathBuf::from("target/release/bookclerkd"),
    ];
    let bin = candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!("bookclerkd binary not found beside bookclerk-tray or on PATH")
        })?;
    Ok(Command::new(bin)
        .env("BOOKCLERK_FILES_DIR", files_dir)
        .env("BOOKCLERK_DAEMON_LISTEN", listen)
        .spawn()?)
}

#[cfg(test)]
mod tests {
    use super::DaemonHandle;

    #[test]
    fn base_url_normalizes_scheme_slash_and_whitespace() {
        assert_eq!(
            DaemonHandle::base_url("127.0.0.1:8787"),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            DaemonHandle::base_url("127.0.0.1:8787/"),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            DaemonHandle::base_url(" http://127.0.0.1:8787/ "),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            DaemonHandle::base_url("https://example.test:9443/"),
            "https://example.test:9443"
        );
    }
}
