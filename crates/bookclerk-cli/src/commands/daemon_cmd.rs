//! `bookclerk daemon` — thin HTTP client for bookclerkd.

use bookclerk_config::Config;
use clap::Subcommand;
use serde_json::Value;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// GET /health
    Health,
    /// GET /status
    Status,
    /// POST /scan
    Scan {
        #[arg(long)]
        account: Option<String>,
    },
    /// POST /acquire
    Acquire {
        #[arg(long)]
        asin: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// GET /jobs
    Jobs,
}

pub async fn run(
    command: DaemonCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let base = daemon_base_url(config);
    match command {
        DaemonCommand::Health => {
            let v = get_json_async(&format!("{base}/health")).await?;
            emit(format, &v, || {
                println!("{}", v["status"].as_str().unwrap_or("ok"));
            })
        }
        DaemonCommand::Status => {
            let v = get_json_async(&format!("{base}/status")).await?;
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
            let v = post_json_async(&format!("{base}/scan"), body).await?;
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
            let v = post_json_async(&format!("{base}/acquire"), body).await?;
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
            let v = get_json_async(&format!("{base}/jobs")).await?;
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

async fn get_json_async(url: &str) -> anyhow::Result<Value> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || get_json(&url))
        .await
        .map_err(|err| anyhow::anyhow!("daemon GET join: {err}"))?
}

async fn post_json_async(url: &str, body: Value) -> anyhow::Result<Value> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || post_json(&url, &body))
        .await
        .map_err(|err| anyhow::anyhow!("daemon POST join: {err}"))?
}

fn get_json(url: &str) -> anyhow::Result<Value> {
    let resp = ureq::get(url)
        .call()
        .map_err(|err| anyhow::anyhow!("daemon GET {url}: {err}"))?;
    resp.into_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}

fn post_json(url: &str, body: &Value) -> anyhow::Result<Value> {
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|err| anyhow::anyhow!("daemon POST {url}: {err}"))?;
    resp.into_json()
        .map_err(|err| anyhow::anyhow!("daemon JSON: {err}"))
}
