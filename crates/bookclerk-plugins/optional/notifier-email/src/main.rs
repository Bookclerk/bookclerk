//! Email notifier guest: serves the `notifier` entrypoint over the plugin
//! protocol (see the crate docs in `lib.rs`).

use async_trait::async_trait;
use bookclerk_plugin_notifier_email::client::HostTransport;
use bookclerk_plugin_notifier_email::notifier::{EmailNotifier, Settings};
use bookclerk_plugin_sdk::{
    manifest_capabilities, serve, Bindings, Entrypoints, Invocation, PluginDescribe, PluginError,
    PluginWorker, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};

/// The guest root.
struct EmailRoot;

#[async_trait(?Send)]
impl PluginWorker for EmailRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "email".into(),
            display_name: Some("Email (SMTP)".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        let settings = bindings.config.json_value().and_then(|config| {
            let secrets = bindings.secrets.json_value()?;
            Settings::read(&config, &secrets, &bindings.network)
        });
        Ok(Entrypoints {
            notifier: Some(Box::new(EmailNotifier::new(settings, HostTransport))),
            ..Entrypoints::default()
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(EmailRoot).await?;
    Ok(())
}
