//! `bookclerk ingress`: the library's public address from an ingress plugin
//! (see `docs/server/ingress.md`), through bookclerkd's `/api/config/ingress`.

use bookclerk_config::Config;
use clap::{Subcommand, ValueEnum};
use serde_json::{json, Value};

use crate::commands::daemon_cmd;
use crate::format_out::{emit, OutputFormat};

/// What public traffic from the ingress may reach.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Exposure {
    /// Only what branches and clerks need: the node API, sign-in and the
    /// clerk routes.
    Branches,
    /// The whole library, web app included.
    Library,
}

impl Exposure {
    /// The API's spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Branches => "branches",
            Self::Library => "library",
        }
    }
}

/// `bookclerk ingress` verbs.
#[derive(Debug, Subcommand)]
pub enum IngressCommand {
    /// Show the active ingress, what it exposes and the public origin.
    Status,
    /// Make a plugin instance the library's active ingress. Needs an Owner
    /// account and operator auth on.
    Activate {
        /// A plugin instance UUID, or a plugin key for its default instance.
        plugin: String,
    },
    /// Turn ingress off. The adopted origin is kept for next time.
    Deactivate,
    /// Choose what public traffic may reach.
    Exposure {
        /// `branches` (the default) or `library`.
        #[arg(value_enum)]
        exposure: Exposure,
    },
    /// Adopt the new origin the active ingress reported. Passkeys and
    /// sign-in redirects registered under the old name stop working.
    #[command(name = "accept-origin")]
    AcceptOrigin,
}

/// Runs an ingress verb against a running bookclerkd.
///
/// # Errors
///
/// Returns an error when the daemon is unreachable or refuses the change.
pub async fn run(
    command: IngressCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let url = format!("{}/api/config/ingress", daemon_cmd::daemon_base_url(config));
    let token = daemon_cmd::operator_bearer(config).await?;
    let change = match command {
        IngressCommand::Status => None,
        IngressCommand::Activate { plugin } => Some(json!({ "activate": plugin })),
        IngressCommand::Deactivate => Some(json!({ "deactivate": true })),
        IngressCommand::Exposure { exposure } => Some(json!({ "exposure": exposure.as_str() })),
        IngressCommand::AcceptOrigin => Some(json!({ "accept_reported_origin": true })),
    };
    let status = match change {
        None => daemon_cmd::get_json_async(&url, token.as_deref()).await?,
        Some(body) => daemon_cmd::post_json_async(&url, body, token.as_deref()).await?,
    };
    emit(format, &status, || print_status(&status))
}

/// Prints the status in the text format.
fn print_status(v: &Value) {
    let text = |key: &str| v[key].as_str().unwrap_or("-").to_string();
    match v["active_instance"].as_str() {
        Some(id) => println!(
            "ingress: on ({} instance {id})",
            v["plugin_key"].as_str().unwrap_or("unknown plugin")
        ),
        None => println!("ingress: off"),
    }
    println!("exposure: {}", text("exposure"));
    println!("origin: {}", text("origin"));
    if let Some(reported) = v["reported_origin"].as_str() {
        println!("reported origin: {reported} (bookclerk ingress accept-origin to adopt it)");
    }
    let overridden = if v["operator_override"].as_bool() == Some(true) {
        " (integrations.public_origin)"
    } else {
        ""
    };
    println!("serving as: {}{overridden}", text("public_origin"));
    if let Some(blocked) = v["blocked"].as_str() {
        println!("blocked: {blocked}");
    }
}
