//! Reference Echo integration — native Rust guest.
//!
//! Speaks Cap'n Proto `api_version = 2` via [`PluginRoot`] + [`Integration`].

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_abi::{
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    HandshakeResult,
};
use bookclerk_plugin_sdk::v2::{
    decode_json, encode_json, DomainEvent, EventResult, HealthOk, Integration, IntegrationContext,
    PluginDescribe, PluginRoot, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{serve, PluginError};

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
                short: Some('m'),
                kind: CliArgKind::String,
                required: false,
                default: Some("hi".into()),
                about: Some("Message to echo".into()),
                positional: false,
            }],
        }],
    }
}

/// Handshake extras stored on `describe.metadata_json` for the host.
fn describe_metadata() -> Result<String, PluginError> {
    encode_json(HandshakeResult {
        api_version: PRODUCT_API_VERSION,
        id: PLUGIN_ID.into(),
        kind: "integration".into(),
        display_name: Some("Echo Integration (native Rust)".into()),
        capabilities: vec![
            "health".into(),
            "diagnose".into(),
            "onEvent".into(),
            "start".into(),
            "cli".into(),
        ],
        cli: Some(cli_schema()),
        ..HandshakeResult::default()
    })
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
            metadata_json: describe_metadata()?,
            ..PluginDescribe::default()
        })
    }

    async fn integration(
        &self,
        _context: IntegrationContext,
    ) -> Result<Box<dyn Integration>, PluginError> {
        Ok(Box::new(EchoIntegration))
    }

    async fn cli_describe(&self) -> Result<String, PluginError> {
        encode_json(cli_schema())
    }

    async fn cli_invoke(&self, params_json: &str) -> Result<String, PluginError> {
        let params: CliInvokeParams = decode_json(params_json)?;
        if params.command != "ping" {
            return encode_json(CliInvokeResult {
                exit_code: 2,
                stdout: String::new(),
                stderr: format!("unknown command {}", params.command),
                json: None,
            });
        }
        let message = params
            .args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("hi");
        encode_json(CliInvokeResult {
            exit_code: 0,
            stdout: format!("pong: {message}\n"),
            stderr: String::new(),
            json: None,
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

    async fn diagnose(&self) -> Result<String, PluginError> {
        encode_json(vec!["echo_native_rust diagnose: ok"])
    }

    async fn on_event(&self, event: DomainEvent) -> Result<EventResult, PluginError> {
        eprintln!("echo_native_rust event: {event:?}");
        Ok(EventResult::Ack)
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
