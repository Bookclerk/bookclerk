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
    let status = match change_body(&command) {
        None => daemon_cmd::get_json_async(&url, token.as_deref()).await?,
        Some(body) => daemon_cmd::post_json_async(&url, body, token.as_deref()).await?,
    };
    emit(format, &status, || print_status(&status))
}

/// The `POST /api/config/ingress` body for `command`; `None` for a read.
fn change_body(command: &IngressCommand) -> Option<Value> {
    match command {
        IngressCommand::Status => None,
        IngressCommand::Activate { plugin } => Some(json!({ "activate": plugin })),
        IngressCommand::Deactivate => Some(json!({ "deactivate": true })),
        IngressCommand::Exposure { exposure } => Some(json!({ "exposure": exposure.as_str() })),
        IngressCommand::AcceptOrigin => Some(json!({ "accept_reported_origin": true })),
    }
}

/// Prints the status in the text format.
fn print_status(v: &Value) {
    for line in status_lines(v) {
        println!("{line}");
    }
}

/// The status as text lines.
fn status_lines(v: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let text = |key: &str| v[key].as_str().unwrap_or("-").to_string();
    match v["active_instance"].as_str() {
        Some(id) => lines.push(format!(
            "ingress: on ({} instance {id})",
            v["plugin_key"].as_str().unwrap_or("unknown plugin")
        )),
        None => lines.push("ingress: off".into()),
    }
    lines.push(format!("exposure: {}", text("exposure")));
    lines.push(format!("origin: {}", text("origin")));
    if let Some(reported) = v["reported_origin"].as_str() {
        lines.push(format!(
            "reported origin: {reported} (bookclerk ingress accept-origin to adopt it)"
        ));
    }
    let overridden = if v["operator_override"].as_bool() == Some(true) {
        " (integrations.public_origin)"
    } else {
        ""
    };
    lines.push(format!("serving as: {}{overridden}", text("public_origin")));
    if let Some(blocked) = v["blocked"].as_str() {
        lines.push(format!("blocked: {blocked}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_verb_sends_one_change() {
        assert_eq!(change_body(&IngressCommand::Status), None);
        assert_eq!(
            change_body(&IngressCommand::Activate {
                plugin: "relay".into()
            }),
            Some(json!({ "activate": "relay" }))
        );
        assert_eq!(
            change_body(&IngressCommand::Deactivate),
            Some(json!({ "deactivate": true }))
        );
        for (exposure, spelling) in [
            (Exposure::Branches, "branches"),
            (Exposure::Library, "library"),
        ] {
            assert_eq!(
                change_body(&IngressCommand::Exposure { exposure }),
                Some(json!({ "exposure": spelling }))
            );
        }
        assert_eq!(
            change_body(&IngressCommand::AcceptOrigin),
            Some(json!({ "accept_reported_origin": true }))
        );
    }

    #[test]
    fn the_status_reads_as_text() {
        let on = json!({
            "active_instance": "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b",
            "plugin_key": "relay",
            "exposure": "branches",
            "origin": "https://a.example",
            "reported_origin": "https://b.example",
            "public_origin": "https://a.example",
            "operator_override": false,
            "blocked": null,
        });
        let lines = status_lines(&on);
        assert_eq!(
            lines[0],
            "ingress: on (relay instance 3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b)"
        );
        assert!(lines.contains(&"exposure: branches".to_string()));
        assert!(lines
            .iter()
            .any(|line| line.starts_with("reported origin: https://b.example")));
        assert!(lines.contains(&"serving as: https://a.example".to_string()));
        print_status(&on);

        let off = json!({
            "active_instance": null,
            "exposure": "library",
            "operator_override": true,
            "public_origin": "https://mine.example",
            "blocked": "no Owner account exists yet",
        });
        let lines = status_lines(&off);
        assert_eq!(lines[0], "ingress: off");
        assert!(lines.contains(&"origin: -".to_string()));
        assert!(lines.contains(
            &"serving as: https://mine.example (integrations.public_origin)".to_string()
        ));
        assert!(lines.contains(&"blocked: no Owner account exists yet".to_string()));
    }
}
