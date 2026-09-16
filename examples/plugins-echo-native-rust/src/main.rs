//! Reference Echo integration — native Rust guest.
//!
//! Speaks Cap'n Proto `api_version = 2` via [`PluginRoot`] + [`Integration`].

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_abi::{
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
};
use bookclerk_plugin_sdk::{serve, PluginError};
use bookclerk_plugin_sdk::{
    DomainEvent, EventResult, HealthOk, Integration, IntegrationContext, PluginDescribe,
    PluginRoot, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};

/// Manifest / describe id for the reference Echo integration (`echo_native_rust`).
const PLUGIN_ID: &str = "echo_native_rust";

/// CLI schema advertised at `cliDescribe` (`ping --message`).
fn cli_schema() -> CliSchema {
    CliSchema {
        commands: vec![CliCommandSpec {
            name: "ping".into(),
            about: Some("Probe echo plugin".into()),
            args: vec![CliArgSpec {
                name: "message".into(),
                long: Some("message".into()),
                short: Some("m".into()),
                kind: CliArgKind::String,
                required: false,
                default: Some("hi".into()),
                about: Some("Message to echo".into()),
                positional: false,
            }],
        }],
    }
}

/// Reference Echo integration guest (health, diagnose, events, and `ping`).
struct EchoRoot;

#[async_trait(?Send)]
impl PluginRoot for EchoRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: PLUGIN_ID.into(),
            kind: "integration".into(),
            display_name: Some("Echo Integration (native Rust)".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["integration".into()],
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "onEvent".into(),
                "start".into(),
                "cli".into(),
            ],
            cli: cli_schema(),
            ..PluginDescribe::default()
        })
    }

    async fn integration(
        &self,
        _context: IntegrationContext,
    ) -> Result<Box<dyn Integration>, PluginError> {
        Ok(Box::new(EchoIntegration))
    }

    async fn cli_describe(&self) -> Result<CliSchema, PluginError> {
        Ok(cli_schema())
    }

    async fn cli_invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        if params.command != "ping" {
            return Ok(CliInvokeResult {
                exit_code: 2,
                stderr: format!("unknown command {}", params.command),
                ..CliInvokeResult::default()
            });
        }
        let message = params
            .args
            .iter()
            .find(|arg| arg.name == "message")
            .map_or("hi", |arg| arg.value.as_str());
        Ok(CliInvokeResult {
            exit_code: 0,
            stdout: format!("pong: {message}\n"),
            ..CliInvokeResult::default()
        })
    }
}

/// Echo integration capability (health, diagnose, events).
struct EchoIntegration;

#[async_trait(?Send)]
impl Integration for EchoIntegration {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "echo_native_rust ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<Vec<String>, PluginError> {
        Ok(vec!["echo_native_rust diagnose: ok".into()])
    }

    async fn on_event(&self, event: DomainEvent) -> Result<EventResult, PluginError> {
        Ok(match event.event_type.as_str() {
            "test_retry" => EventResult::Retry {
                retry_at_unix_ms: 1,
                reason: "echo retry".into(),
            },
            "test_reject" => EventResult::Reject {
                reason: "echo reject".into(),
            },
            "test_dead_letter" => EventResult::DeadLetter {
                reason: "echo dead letter".into(),
            },
            "test_suspend" => EventResult::Suspended {
                checkpoint_json: r#"{"n":1}"#.into(),
                checkpoint_schema_version: 1,
                wake_at_unix_ms: 1,
                wake_on_event_type: String::new(),
                wake_on_filter_json: String::new(),
            },
            _ => EventResult::Ack,
        })
    }

    async fn start(&self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(EchoRoot).await?;
    Ok(())
}
