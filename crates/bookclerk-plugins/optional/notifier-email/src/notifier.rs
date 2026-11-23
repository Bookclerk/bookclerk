//! The instance's settings and the `notifier` entrypoint.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    HealthOk, NetworkGrant, Notifier, NotifyDestinationKind, NotifySendParams, NotifySent,
    NotifyValidateParams, NotifyValidated, PluginError, SettingGrantState,
};
use serde_json::Value;

use crate::client::{converse, Transport, STEP_TIMEOUT};
use crate::compose::{compose, Envelope};
use crate::smtp::{
    parse_address, parse_email, parse_recipients, rfc5322_date, split_host_port, Credentials,
    HostPort, Security, Session, SessionOptions,
};

/// The key of the setting that names the server.
pub const SERVER_SETTING: &str = "server";

/// The instance's settings, checked.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Where the server is, as the host approved it.
    pub server: HostPort,
    /// How the connection is protected.
    pub security: Security,
    /// Sign-in, when a user name is set.
    pub credentials: Option<Credentials>,
    /// Sender address.
    pub from: String,
    /// Sender display name.
    pub from_name: String,
    /// The `to` setting, unparsed; empty when only patrons' addresses are
    /// used.
    pub to: String,
}

/// A string setting from `CONFIG`, or empty.
///
/// # Arguments
///
/// * `config` - The decoded `CONFIG` document.
/// * `key` - The setting.
fn string_setting<'a>(config: &'a Value, key: &str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or_default()
}

impl Settings {
    /// Reads and checks the settings.
    ///
    /// # Arguments
    ///
    /// * `config` - The decoded `CONFIG` document.
    /// * `secrets` - The decoded `SECRETS` document.
    /// * `network` - The network grant in effect.
    ///
    /// # Errors
    ///
    /// `invalid_params` when a setting is missing or malformed;
    /// `network_not_granted` while the server's destination awaits
    /// approval.
    pub fn read(
        config: &Value,
        secrets: &Value,
        network: &NetworkGrant,
    ) -> Result<Self, PluginError> {
        let grant = network
            .settings
            .iter()
            .find(|setting| setting.key == SERVER_SETTING);
        let destination = match grant.map(|grant| (grant.state, grant.destination.as_str())) {
            None | Some((SettingGrantState::Unset, _)) => {
                return Err(PluginError::invalid_params(SERVER_NOT_SET));
            }
            Some((SettingGrantState::Invalid, _)) => {
                return Err(PluginError::invalid_params(
                    "the server setting names no usable host and port",
                ));
            }
            Some((SettingGrantState::Pending, _)) => {
                return Err(PluginError::new(
                    bookclerk_plugin_sdk::PluginErrorCode::NetworkNotGranted,
                    "the server's destination awaits approval in the plugin's network settings",
                ));
            }
            Some((SettingGrantState::Approved, destination)) => destination,
        };
        let security = match string_setting(config, "security") {
            "" => Security::StartTls,
            value => Security::parse(value)?,
        };
        let username = string_setting(config, "username").trim();
        let credentials = if username.is_empty() {
            None
        } else {
            Some(Credentials {
                username: username.to_owned(),
                password: secrets
                    .get("password")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        };
        if credentials.is_some() && security == Security::None {
            return Err(PluginError::invalid_params(
                "a user name and password need security starttls or tls",
            ));
        }
        Ok(Self {
            server: split_host_port(destination)?,
            security,
            credentials,
            from: parse_email(
                string_setting(config, "from_address"),
                "the from_address setting",
            )?,
            from_name: string_setting(config, "from_name").to_owned(),
            to: string_setting(config, "to").to_owned(),
        })
    }
}

/// Why the settings are unusable when no mail server is set: a fresh
/// install, which `health` reports as healthy but not configured yet.
const SERVER_NOT_SET: &str = "the server setting is not set";

/// `notifier` entrypoint: one SMTP submission per message.
pub struct EmailNotifier<T> {
    /// The settings, or why they are unusable (reported by `health` and
    /// every `send`).
    settings: Result<Settings, PluginError>,
    /// Where connections come from.
    transport: T,
    /// A fresh MIME boundary per message.
    boundary: Box<dyn Fn() -> String>,
    /// The current time, Unix seconds.
    now: Box<dyn Fn() -> i64>,
}

impl<T: Transport> EmailNotifier<T> {
    /// Builds the entrypoint from the instance's bindings.
    ///
    /// # Arguments
    ///
    /// * `settings` - [`Settings::read`]'s answer.
    /// * `transport` - Where connections come from.
    #[must_use]
    pub fn new(settings: Result<Settings, PluginError>, transport: T) -> Self {
        Self {
            settings,
            transport,
            boundary: Box::new(|| format!("=_bookclerk_{}", uuid::Uuid::new_v4().simple())),
            now: Box::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_secs()).ok())
                    .unwrap_or_default()
            }),
        }
    }

    /// Fixes the boundary and clock, so tests see the same message every
    /// run.
    ///
    /// # Arguments
    ///
    /// * `boundary` - The MIME boundary.
    /// * `now` - The time, Unix seconds.
    #[must_use]
    pub fn with_fixed(mut self, boundary: &str, now: i64) -> Self {
        let boundary = boundary.to_owned();
        self.boundary = Box::new(move || boundary.clone());
        self.now = Box::new(move || now);
        self
    }

    /// The settings, or why they are unusable.
    ///
    /// # Errors
    ///
    /// The reason the settings were refused.
    fn settings(&self) -> Result<&Settings, PluginError> {
        self.settings.as_ref().map_err(Clone::clone)
    }
}

#[async_trait(?Send)]
impl<T: Transport> Notifier for EmailNotifier<T> {
    /// Checks the settings without connecting, so a probe never signs in.
    async fn health(&self) -> Result<HealthOk, PluginError> {
        let checked = self.settings().and_then(|settings| {
            if settings.to.trim().is_empty() {
                Ok(())
            } else {
                parse_recipients(&settings.to).map(|_| ())
            }
        });
        Ok(match checked {
            Ok(()) => HealthOk {
                ok: true,
                detail: String::new(),
            },
            Err(err) if err.message == SERVER_NOT_SET => HealthOk {
                ok: true,
                detail: "no mail server set yet".to_owned(),
            },
            Err(err) => HealthOk {
                ok: false,
                detail: err.message,
            },
        })
    }

    async fn send(&self, params: NotifySendParams) -> Result<NotifySent, PluginError> {
        let settings = self.settings()?;
        let recipients = match params.destination.kind {
            NotifyDestinationKind::Personal => {
                vec![parse_address(&params.destination.address_json)?.email]
            }
            NotifyDestinationKind::Channel => parse_recipients(&settings.to)?,
        };
        let date = rfc5322_date((self.now)());
        let boundary = (self.boundary)();
        let message = compose(
            &params.message,
            &Envelope {
                from: &settings.from,
                from_name: &settings.from_name,
                to: &recipients,
                destination_id: &params.destination.destination_id,
                date: &date,
                boundary: &boundary,
            },
        );
        let helo_name = settings
            .from
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain)
            .to_owned();
        let mut session = Session::new(SessionOptions {
            security: settings.security,
            helo_name,
            credentials: settings.credentials.clone(),
            from: settings.from.clone(),
            recipients,
            message,
        })?;
        converse(
            &self.transport,
            &settings.server,
            settings.security,
            &mut session,
            STEP_TIMEOUT,
        )
        .await?;
        Ok(NotifySent { provider_id: None })
    }

    /// Checks a patron's address without connecting.
    async fn validate_destination(
        &self,
        params: NotifyValidateParams,
    ) -> Result<NotifyValidated, PluginError> {
        let address = parse_address(&params.address_json)?;
        Ok(NotifyValidated {
            label: Some(address.email.clone()),
            address_json: address.address_json,
        })
    }
}

#[cfg(test)]
mod tests;
