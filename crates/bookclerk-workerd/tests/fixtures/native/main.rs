//! Native contract fixture: the Cap'n Proto twin of the workerd `storefront`
//! and `stream` author fixtures.
//!
//! The conformance suite runs the same storefront, CLI, and job vectors
//! against this guest directly (diagnostic transport) and behind the
//! `bookclerk-workerd` front door (typed passthrough), and against the
//! workerd author isolate. Every branch here mirrors one branch of the
//! JavaScript fixtures so the three paths must agree byte-for-byte on
//! defaults, optional results, and typed errors.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_abi::{
    serve_plugin_stdio, Bindings, CatalogDetailParams, CatalogHit, CliCommandSpec, CliInvokeParams,
    CliInvokeResult, CliSchema, ContentSource, Entrypoints, HealthOk, Invocation, JobController,
    JobOutcome, JobRunner, LoginParams, LoginResult, PluginCli, PluginDescribe, PluginError,
    PluginErrorCode, PluginWorker, PurchaseHint, PurchaseHintParams, Result, ScalarLimits,
    SearchCatalogParams, SourceAccount, WriteOptions, FEATURE_SCALAR_LIMITS, FEATURE_STREAMS,
    MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION,
};
use bookclerk_plugin_manifest::PluginManifest;
use serde::Deserialize;

/// Manifest shipped beside this fixture; `describe()` must echo its capabilities.
const MANIFEST: &str = include_str!("plugin.toml");

/// Manifest id (the launcher and the tests pin `describe().id` to it).
const PLUGIN_ID: &str = "native_fixture";

struct FixtureRoot;

#[async_trait(?Send)]
impl PluginWorker for FixtureRoot {
    async fn describe(&self) -> Result<PluginDescribe> {
        let manifest = PluginManifest::parse(MANIFEST)
            .map_err(|err| PluginError::internal(format!("plugin.toml: {err}")))?;
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: PLUGIN_ID.into(),
            display_name: Some("Native contract fixture".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into(), FEATURE_STREAMS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest.capabilities(),
            ..PluginDescribe::default()
        })
    }

    async fn open(&self, _invocation: Invocation, bindings: Bindings) -> Result<Entrypoints> {
        Ok(Entrypoints {
            storefront: Some(Box::new(FixtureStorefront {
                env: granted_env(&bindings),
            })),
            cli: Some(Box::new(FixtureCli)),
            job_runner: Some(Box::new(FixtureJobs)),
            ..Entrypoints::default()
        })
    }
}

/// Names of the bindings the host granted, sorted like the isolate's `env` keys.
fn granted_env(bindings: &Bindings) -> String {
    let mut names = vec!["CONFIG".to_string(), "SECRETS".to_string()];
    if bindings.events.is_some() {
        names.push("EVENTS".into());
    }
    if bindings.storage.is_some() {
        names.push("WORK_FS".into());
    }
    names.extend(bindings.databases.iter().map(|(name, _)| name.clone()));
    names.sort();
    names.join(",")
}

struct FixtureStorefront {
    env: String,
}

#[async_trait(?Send)]
impl ContentSource for FixtureStorefront {
    async fn health(&self) -> Result<HealthOk> {
        Ok(HealthOk {
            ok: true,
            detail: format!("env={}", self.env),
        })
    }

    async fn diagnose(&self) -> Result<Vec<String>> {
        Ok(vec!["line one".into(), "line two".into()])
    }

    async fn list_accounts(&self) -> Result<Vec<SourceAccount>> {
        Ok(vec![
            SourceAccount {
                account_id: "acct-1".into(),
                source: PLUGIN_ID.into(),
                marketplace: "us".into(),
                label: Some("One".into()),
                scan_enabled: true,
            },
            SourceAccount {
                account_id: "acct-2".into(),
                source: PLUGIN_ID.into(),
                marketplace: "uk".into(),
                ..SourceAccount::default()
            },
        ])
    }

    async fn search_catalog(&self, params: SearchCatalogParams) -> Result<Vec<CatalogHit>> {
        Ok(vec![CatalogHit {
            product_id: format!("hit:{}:{}:{}", params.query, params.limit, params.page),
            title: params.query,
            authors: Some("A. Author".into()),
            ..CatalogHit::default()
        }])
    }

    async fn purchase_hint(&self, _params: PurchaseHintParams) -> Result<Option<PurchaseHint>> {
        Ok(None)
    }

    async fn catalog_detail(&self, params: CatalogDetailParams) -> Result<Option<CatalogHit>> {
        if params.product_id == "missing" {
            return Ok(None);
        }
        Ok(Some(CatalogHit {
            title: format!("Detail {}", params.product_id),
            product_id: params.product_id,
            ..CatalogHit::default()
        }))
    }

    async fn login(&self, params: LoginParams) -> Result<LoginResult> {
        Err(PluginError::new(
            PluginErrorCode::Unauthorized,
            format!("no credentials for {}", params.marketplace),
        ))
    }
}

struct FixtureCli;

#[async_trait(?Send)]
impl PluginCli for FixtureCli {
    async fn describe(&self) -> Result<CliSchema> {
        Ok(CliSchema {
            commands: vec![CliCommandSpec {
                name: "echo".into(),
                about: Some("Echo the arguments".into()),
                args: Vec::new(),
            }],
        })
    }

    async fn invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult> {
        let args = params
            .args
            .iter()
            .map(|arg| format!("{}={}", arg.name, arg.value))
            .collect::<Vec<_>>()
            .join(";");
        Ok(CliInvokeResult {
            exit_code: 3,
            stdout: format!("{}:{args}", params.command),
            ..CliInvokeResult::default()
        })
    }
}

/// Job payload shared with the workerd `stream` fixture.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct CopySpec {
    from: String,
    to: String,
    await_cancel: bool,
}

struct FixtureJobs;

#[async_trait(?Send)]
impl JobRunner for FixtureJobs {
    async fn job(&self, controller: JobController) -> Result<JobOutcome> {
        let JobController {
            invocation,
            input,
            output,
            progress,
            cancel,
        } = controller;
        let spec: CopySpec = serde_json::from_str(&invocation.payload_json)
            .map_err(|err| PluginError::invalid_params(format!("job payload: {err}")))?;
        if spec.await_cancel {
            progress.report(0.0, "waiting for cancel").await?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
            while !cancel.poll().await? {
                if tokio::time::Instant::now() >= deadline {
                    return Err(PluginError::internal("cancel never arrived"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            return Ok(JobOutcome::Cancelled {
                message: "host cancelled the copy".into(),
            });
        }
        progress.report(0.0, "opening").await?;
        let opened = input.open(&spec.from).await?;
        progress.report(10.0, "copying").await?;
        let put = output
            .put(
                &spec.to,
                opened.body,
                WriteOptions {
                    content_type: opened.meta.content_type.clone(),
                    content_length: (opened.meta.size > 0).then_some(opened.meta.size),
                    ..WriteOptions::default()
                },
            )
            .await?;
        progress.report(100.0, "done").await?;
        Ok(JobOutcome::Completed {
            message: format!("copied {} -> {}", spec.from, spec.to),
            bytes_copied: put.bytes_written,
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(serve_plugin_stdio(
            std::sync::Arc::new(FixtureRoot),
            MAX_STREAM_WINDOW_BYTES,
        ))
        .await?;
    Ok(())
}
