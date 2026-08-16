//! `bookclerk daemon` — thin HTTP client for bookclerkd.

use bookclerk_config::Config;
use bookclerk_library::{
    configure_master_key_with, resolve_operator_token, rotate_operator_token, ResolveOperatorToken,
};
use clap::Subcommand;
use serde_json::Value;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk daemon` verbs that call bookclerkd or manage the operator token.
pub enum DaemonCommand {
    /// GET /health
    Health,
    /// GET /status (or /api/status)
    Status,
    /// POST /scan (or /api/library/scan)
    Scan {
        #[arg(long)]
        /// Optional account id forwarded in the scan JSON body.
        account: Option<String>,
    },
    /// POST /acquire (or /api/library/acquire)
    Acquire {
        #[arg(long)]
        /// Optional product id (ASIN/ISBN) forwarded in the acquire JSON body.
        asin: Option<String>,
        #[arg(long)]
        /// Optional account id forwarded in the acquire JSON body.
        account: Option<String>,
    },
    /// GET /jobs (or /api/jobs)
    Jobs,
    /// Show or rotate the operator API token (DB-backed; env override wins).
    Token {
        #[command(subcommand)]
        /// Nested token verb; omitted defaults to [`TokenCommand::Show`].
        command: Option<TokenCommand>,
    },
}

#[derive(Debug, Subcommand)]
/// Operator-token subcommands: print the effective token or mint a new one.
pub enum TokenCommand {
    /// Print the current effective operator token (default).
    Show,
    /// Mint a new token, store it in encrypted_secrets, and reload the daemon when reachable.
    Rotate,
}

/// Dispatches a daemon verb: token ops stay local; others call the control plane.
pub async fn run(
    command: DaemonCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if let DaemonCommand::Token { command } = command {
        return run_token(command.unwrap_or(TokenCommand::Show), config, format).await;
    }

    let base = daemon_base_url(config);
    let token = operator_bearer(config).await?;
    match command {
        DaemonCommand::Health => {
            let v = get_json_async(&format!("{base}/health"), None).await?;
            emit(format, &v, || {
                println!("{}", v["status"].as_str().unwrap_or("ok"));
            })
        }
        DaemonCommand::Status => {
            let v = get_json_async(&format!("{base}/api/status"), token.as_deref()).await?;
            emit(format, &v, || {
                println!(
                    "accounts={} books={} acquired={} pending={} error={} in_progress={} listen={} storage={} auth_enabled={}",
                    v["accounts"],
                    v["books"],
                    v["acquired"],
                    v["pending"],
                    v["error"],
                    v["in_progress"],
                    v["listen"].as_str().unwrap_or("-"),
                    v["storage_backend"].as_str().unwrap_or("-"),
                    v["auth_enabled"],
                );
            })
        }
        DaemonCommand::Scan { account } => {
            let body = serde_json::json!({ "account": account });
            let v = post_json_async(&format!("{base}/api/library/scan"), body, token.as_deref())
                .await?;
            emit(format, &v, || {
                println!(
                    "ok={} job_id={} {}",
                    v["ok"],
                    v["job_id"].as_str().unwrap_or("-"),
                    v["message"].as_str().unwrap_or("")
                );
            })
        }
        DaemonCommand::Acquire { asin, account } => {
            let body = serde_json::json!({ "asin": asin, "account": account });
            let v = post_json_async(
                &format!("{base}/api/library/acquire"),
                body,
                token.as_deref(),
            )
            .await?;
            emit(format, &v, || {
                println!(
                    "ok={} job_id={} {}",
                    v["ok"],
                    v["job_id"].as_str().unwrap_or("-"),
                    v["message"].as_str().unwrap_or("")
                );
            })
        }
        DaemonCommand::Jobs => {
            let v = get_json_async(&format!("{base}/api/jobs"), token.as_deref()).await?;
            emit(format, &v, || {
                let jobs = v.as_array().cloned().unwrap_or_default();
                if jobs.is_empty() {
                    println!("no jobs");
                    return;
                }
                for job in jobs {
                    println!(
                        "{} kind={} status={} {}",
                        job["id"].as_str().unwrap_or("-"),
                        job["kind"].as_str().unwrap_or("-"),
                        job["status"].as_str().unwrap_or("-"),
                        job["detail"].as_str().unwrap_or("")
                    );
                }
            })
        }
        DaemonCommand::Token { .. } => unreachable!(),
    }
}

/// Shows or rotates the operator token; rotate also tries `POST /api/config/reload`.
async fn run_token(
    command: TokenCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if !config.daemon.auth.enabled {
        anyhow::bail!("daemon.auth.enabled is false; operator token is not required");
    }
    configure_master_key_with(&config.paths().files_dir, config.auth_password().as_deref())?;
    let store = crate::registry::open_library(config).await?;

    match command {
        TokenCommand::Show => {
            let Some((token, how)) = resolve_operator_token(config, store.db(), false).await?
            else {
                anyhow::bail!(
                    "no operator token available; start bookclerkd once to mint one, \
                     or set BOOKCLERK_OPERATOR_TOKEN"
                );
            };
            let source = match how {
                ResolveOperatorToken::Env => "env",
                ResolveOperatorToken::Database => "database",
                ResolveOperatorToken::LegacyFile => "legacy-file",
                ResolveOperatorToken::Generated => "generated",
            };
            emit(
                format,
                &serde_json::json!({ "token": token, "source": source }),
                || {
                    println!("{token}");
                },
            )
        }
        TokenCommand::Rotate => {
            let old_bearer = resolve_operator_token(config, store.db(), false)
                .await?
                .map(|(t, _)| t);
            let token = rotate_operator_token(store.db()).await?;
            let mut reloaded = false;
            let mut reload_error: Option<String> = None;
            if let Some(old) = old_bearer.as_deref() {
                let base = daemon_base_url(config);
                match post_json_async(
                    &format!("{base}/api/config/reload"),
                    serde_json::json!({}),
                    Some(old),
                )
                .await
                {
                    Ok(_) => reloaded = true,
                    Err(err) => reload_error = Some(err.to_string()),
                }
            }
            emit(
                format,
                &serde_json::json!({
                    "token": token,
                    "reloaded": reloaded,
                    "reload_error": reload_error,
                    "note": "previous sessions are invalidated after a successful reload",
                }),
                || {
                    println!("{token}");
                    if reloaded {
                        eprintln!("bookclerk: daemon reloaded with the new operator token");
                    } else if let Some(err) = &reload_error {
                        eprintln!(
                            "bookclerk: token rotated in the database, but daemon reload failed \
                             ({err}); send SIGHUP or POST /api/config/reload after restart"
                        );
                    } else {
                        eprintln!(
                            "bookclerk: token rotated in the database; reload or restart \
                             bookclerkd to apply it"
                        );
                    }
                },
            )
        }
    }
}

/// HTTP origin for bookclerkd, derived from `daemon.listen` (tray/CLI default).
fn daemon_base_url(config: &Config) -> String {
    config.daemon.listen.tray_base_url()
}

/// Print a loopback operator sign-in URL (requires a running daemon).
///
/// Uses the same `POST /api/auth/tray-handoff/prepare` ticket as the tray Copy
/// sign-in link. Does not print the durable operator token.
pub async fn run_login(config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    if !config.daemon.auth.enabled {
        anyhow::bail!("daemon.auth.enabled is false; a sign-in link is not required");
    }
    let base = daemon_base_url(config);
    let token = operator_bearer(config).await?;
    let prepare_url = format!("{base}/api/auth/tray-handoff/prepare");
    let body = post_prepare_async(&prepare_url, token.as_deref()).await?;
    let (code, expires_in_secs) = parse_prepare_response(&body)?;
    let url = sign_in_url(&base, &code);
    emit(
        format,
        &serde_json::json!({ "url": url, "expires_in_secs": expires_in_secs }),
        || {
            println!("{url}");
        },
    )
}

/// `GET /api/auth/tray-handoff?code=` URL on the daemon's tray/loopback origin.
fn sign_in_url(base: &str, code: &str) -> String {
    format!(
        "{}/api/auth/tray-handoff?code={code}",
        base.trim_end_matches('/')
    )
}

/// Read the one-time code and advertised TTL from a prepare JSON body.
fn parse_prepare_response(v: &Value) -> anyhow::Result<(String, u64)> {
    let code = v.get("code").and_then(Value::as_str).unwrap_or("").trim();
    anyhow::ensure!(!code.is_empty(), "daemon returned an empty handoff code");
    let expires = v
        .get("expires_in_secs")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .unwrap_or(bookclerk_config::TRAY_HANDOFF_TTL_SECS_DEFAULT)
        .clamp(
            bookclerk_config::TRAY_HANDOFF_TTL_SECS_MIN,
            bookclerk_config::TRAY_HANDOFF_TTL_SECS_MAX,
        );
    Ok((code.to_string(), expires))
}

/// Runs a blocking prepare POST on a worker thread and parses the JSON body.
async fn post_prepare_async(url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let url = url.to_string();
    let bearer = bearer.map(str::to_owned);
    tokio::task::spawn_blocking(move || post_prepare(&url, bearer.as_deref()))
        .await
        .map_err(|err| anyhow::anyhow!("daemon POST join: {err}"))?
}

/// Synchronous prepare POST; maps 403 / connection failures to operator-facing errors.
fn post_prepare(url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let mut req = ureq::post(url).header("Content-Type", "application/json");
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = req
        .send_json(serde_json::json!({}))
        .map_err(|err| map_prepare_error(err, url))?;
    if response.status().as_u16() == 204 {
        anyhow::bail!("daemon.auth.enabled is false; a sign-in link is not required");
    }
    response
        .body_mut()
        .read_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}

/// Turn a ureq prepare failure into a clear "daemon down" or "not loopback" error.
fn map_prepare_error(err: ureq::Error, url: &str) -> anyhow::Error {
    match err {
        ureq::Error::StatusCode(403) => anyhow::anyhow!(
            "sign-in link refused (403): prepare requires a direct loopback connection to {url}"
        ),
        ureq::Error::StatusCode(code) => anyhow::anyhow!("daemon POST {url}: HTTP {code}"),
        other => anyhow::anyhow!(
            "daemon is not running at {url} ({other}); start bookclerkd then retry `bookclerk login`"
        ),
    }
}

/// Resolves the operator bearer when daemon auth is enabled; missing tokens fail closed.
async fn operator_bearer(config: &Config) -> anyhow::Result<Option<String>> {
    if !config.daemon.auth.enabled {
        return Ok(None);
    }
    configure_master_key_with(&config.paths().files_dir, config.auth_password().as_deref())?;
    let store = crate::registry::open_library(config).await?;
    match resolve_operator_token(config, store.db(), false).await? {
        Some((token, _)) => Ok(Some(token)),
        None => anyhow::bail!(
            "daemon auth is enabled but no operator token is available; set \
             BOOKCLERK_OPERATOR_TOKEN or run `bookclerk daemon token` after starting bookclerkd"
        ),
    }
}

/// Runs a blocking GET on a worker thread and parses the JSON body.
async fn get_json_async(url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let url = url.to_string();
    let bearer = bearer.map(str::to_owned);
    tokio::task::spawn_blocking(move || get_json(&url, bearer.as_deref()))
        .await
        .map_err(|err| anyhow::anyhow!("daemon GET join: {err}"))?
}

/// Runs a blocking JSON POST on a worker thread and parses the JSON body.
async fn post_json_async(url: &str, body: Value, bearer: Option<&str>) -> anyhow::Result<Value> {
    let url = url.to_string();
    let bearer = bearer.map(str::to_owned);
    tokio::task::spawn_blocking(move || post_json(&url, body, bearer.as_deref()))
        .await
        .map_err(|err| anyhow::anyhow!("daemon POST join: {err}"))?
}

/// Synchronous GET against bookclerkd, attaching a bearer token when present.
fn get_json(url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let mut req = ureq::get(url);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = req
        .call()
        .map_err(|err| anyhow::anyhow!("daemon GET {url}: {err}"))?;
    response
        .body_mut()
        .read_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}

/// Synchronous JSON POST against bookclerkd (`Content-Type: application/json`).
fn post_json(url: &str, body: Value, bearer: Option<&str>) -> anyhow::Result<Value> {
    let mut req = ureq::post(url).header("Content-Type", "application/json");
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = req
        .send_json(&body)
        .map_err(|err| anyhow::anyhow!("daemon POST {url}: {err}"))?;
    response
        .body_mut()
        .read_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_url_is_loopback_code_query() {
        assert_eq!(
            sign_in_url("http://localhost:8787/", "abc"),
            "http://localhost:8787/api/auth/tray-handoff?code=abc"
        );
        assert!(!sign_in_url("http://localhost:8787", "abc").contains("token="));
    }

    #[test]
    fn parse_prepare_response_requires_code_and_ttl() {
        let (code, ttl) = parse_prepare_response(&serde_json::json!({
            "code": "deadbeef",
            "expires_in_secs": 180
        }))
        .unwrap();
        assert_eq!(code, "deadbeef");
        assert_eq!(ttl, 180);

        let (_, ttl) = parse_prepare_response(&serde_json::json!({ "code": "aa" })).unwrap();
        assert_eq!(ttl, 180);

        let (_, ttl) =
            parse_prepare_response(&serde_json::json!({ "code": "aa", "expires_in_secs": 10 }))
                .unwrap();
        assert_eq!(ttl, 30);

        assert!(parse_prepare_response(&serde_json::json!({ "code": "" })).is_err());
        assert!(parse_prepare_response(&serde_json::json!({})).is_err());
    }
}
