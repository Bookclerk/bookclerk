//! `bookclerk daemon` — thin HTTP client for bookclerkd.

use bookclerk_config::{read_operator_token, Config};
use clap::Subcommand;
use serde_json::Value;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// GET /health
    Health,
    /// GET /status (or /api/status)
    Status,
    /// POST /scan (or /api/library/scan)
    Scan {
        #[arg(long)]
        account: Option<String>,
    },
    /// POST /acquire (or /api/library/acquire)
    Acquire {
        #[arg(long)]
        asin: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// GET /jobs (or /api/jobs)
    Jobs,
}

pub async fn run(
    command: DaemonCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let base = daemon_base_url(config);
    let token = operator_bearer(config)?;
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
                    "accounts={} books={} acquired={} pending={} error={} in_progress={} listen={} storage={}",
                    v["accounts"],
                    v["books"],
                    v["acquired"],
                    v["pending"],
                    v["error"],
                    v["in_progress"],
                    v["listen"].as_str().unwrap_or("-"),
                    v["storage_backend"].as_str().unwrap_or("-"),
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
    }
}

fn daemon_base_url(config: &Config) -> String {
    let listen = config.daemon.listen.trim();
    if listen.starts_with("http://") || listen.starts_with("https://") {
        listen.trim_end_matches('/').to_string()
    } else {
        format!("http://{listen}")
    }
}

fn operator_bearer(config: &Config) -> anyhow::Result<Option<String>> {
    if !config.daemon.auth.enabled {
        return Ok(None);
    }
    match read_operator_token(config)? {
        Some((token, _)) => Ok(Some(token)),
        None => anyhow::bail!(
            "daemon auth is enabled but no operator token is available; set \
             BOOKCLERK_OPERATOR_TOKEN or create {}",
            bookclerk_config::operator_token_path(config).display()
        ),
    }
}

async fn get_json_async(url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let url = url.to_string();
    let bearer = bearer.map(str::to_string);
    tokio::task::spawn_blocking(move || get_json(&url, bearer.as_deref()))
        .await
        .map_err(|err| anyhow::anyhow!("daemon GET join: {err}"))?
}

async fn post_json_async(url: &str, body: Value, bearer: Option<&str>) -> anyhow::Result<Value> {
    let url = url.to_string();
    let bearer = bearer.map(str::to_string);
    tokio::task::spawn_blocking(move || post_json(&url, &body, bearer.as_deref()))
        .await
        .map_err(|err| anyhow::anyhow!("daemon POST join: {err}"))?
}

fn get_json(url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let mut req = ureq::get(url);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut resp = req
        .call()
        .map_err(|err| anyhow::anyhow!("daemon GET {url}: {err}"))?;
    resp.body_mut()
        .read_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}

fn post_json(url: &str, body: &Value, bearer: Option<&str>) -> anyhow::Result<Value> {
    let mut req = ureq::post(url).header("Content-Type", "application/json");
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut resp = req
        .send_json(body)
        .map_err(|err| anyhow::anyhow!("daemon POST {url}: {err}"))?;
    resp.body_mut()
        .read_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}
