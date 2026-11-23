//! The instance's settings and the `notifier` entrypoint.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    HealthOk, NetworkGrant, NotificationMessage, Notifier, NotifyCheckOutcome,
    NotifyDeliverabilityAdvice, NotifyDeliverabilityParams, NotifyDeliverabilityReport,
    NotifyDestinationKind, NotifySendParams, NotifySent, NotifyValidateParams, NotifyValidated,
    PluginError, SettingGrantState,
};
use serde_json::Value;

use crate::checkers::{Checkers, HttpCheckers};
use crate::client::{converse, Transport, STEP_TIMEOUT};
use crate::compose::{compose, Envelope};
use crate::deliverability::{
    dns_findings, mail_tester_findings, merge_advice, parse_doh_txt, parse_mail_tester,
    parse_spamcheck, postmark_findings, MailTesterAnswer, MAIL_TESTER, MAIL_TESTER_DOMAIN,
    MAIL_TESTER_URL,
};
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
    /// The operator's mail-tester.com account name, checked; `None` skips
    /// that checker.
    pub mail_tester_account: Option<String>,
}

/// Longest mail-tester account name accepted.
const MAX_MAIL_TESTER_ACCOUNT_CHARS: usize = 40;

/// Reads the `mail_tester_account` setting: empty is `None`; otherwise
/// ASCII letters, digits, `.`, `_` and `-`, at most
/// [`MAX_MAIL_TESTER_ACCOUNT_CHARS`].
///
/// # Arguments
///
/// * `value` - The setting.
///
/// # Errors
///
/// `invalid_params` for any other value.
fn mail_tester_account(value: &str) -> Result<Option<String>, PluginError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > MAX_MAIL_TESTER_ACCOUNT_CHARS
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(PluginError::invalid_params(
            "the mail_tester_account setting holds characters an account name can't",
        ));
    }
    Ok(Some(value.to_owned()))
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
            mail_tester_account: mail_tester_account(string_setting(
                config,
                "mail_tester_account",
            ))?,
        })
    }
}

/// Most times mail-tester is asked for a result.
const MAIL_TESTER_POLLS: usize = 12;

/// Wait before each mail-tester poll: 12 polls take a minute, inside the
/// ABI's two-minute deadline with the other checks.
const MAIL_TESTER_POLL_GAP: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// The deliverability checkers.
    checkers: Box<dyn Checkers>,
    /// A fresh mail-tester token per check.
    token: Box<dyn Fn() -> String>,
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
            checkers: Box::new(HttpCheckers),
            token: Box::new(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()),
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

    /// Uses `checkers` and a fixed mail-tester token, for tests.
    ///
    /// # Arguments
    ///
    /// * `checkers` - Canned checker answers.
    /// * `token` - The mail-tester token.
    #[must_use]
    pub fn with_checkers(mut self, checkers: Box<dyn Checkers>, token: &str) -> Self {
        let token = token.to_owned();
        self.checkers = checkers;
        self.token = Box::new(move || token.clone());
        self
    }

    /// Composes `message` for `recipients` and submits it to the server.
    ///
    /// # Arguments
    ///
    /// * `settings` - The checked settings.
    /// * `message` - The rendered message.
    /// * `recipients` - Checked addresses.
    /// * `destination_id` - Host id of the destination, for the
    ///   `Message-ID`.
    ///
    /// # Errors
    ///
    /// What the session or the connection reported.
    async fn submit(
        &self,
        settings: &Settings,
        message: &NotificationMessage,
        recipients: Vec<String>,
        destination_id: &str,
    ) -> Result<(), PluginError> {
        let composed = self.compose_for(settings, message, &recipients, destination_id);
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
            message: composed,
        })?;
        converse(
            &self.transport,
            &settings.server,
            settings.security,
            &mut session,
            STEP_TIMEOUT,
        )
        .await
    }

    /// The whole email for `recipients`.
    ///
    /// # Arguments
    ///
    /// * `settings` - The checked settings.
    /// * `message` - The rendered message.
    /// * `recipients` - Checked addresses.
    /// * `destination_id` - Host id of the destination.
    fn compose_for(
        &self,
        settings: &Settings,
        message: &NotificationMessage,
        recipients: &[String],
        destination_id: &str,
    ) -> String {
        let date = rfc5322_date((self.now)());
        let boundary = (self.boundary)();
        compose(
            message,
            &Envelope {
                from: &settings.from,
                from_name: &settings.from_name,
                to: recipients,
                destination_id,
                date: &date,
                boundary: &boundary,
            },
        )
    }

    /// Sends a test to mail-tester and polls for its result.
    ///
    /// # Arguments
    ///
    /// * `settings` - The checked settings.
    /// * `message` - The host's sample.
    /// * `account` - The operator's mail-tester account.
    ///
    /// # Returns
    ///
    /// The result page and the last answer, or why it could not run.
    async fn mail_tester_run(
        &self,
        settings: &Settings,
        message: &NotificationMessage,
        account: &str,
    ) -> (String, Result<Option<MailTesterAnswer>, String>) {
        let id = format!("{account}-{}", (self.token)());
        let report = format!("{MAIL_TESTER_URL}/{id}");
        let address = match parse_email(&format!("{id}@{MAIL_TESTER_DOMAIN}"), "the test address") {
            Ok(address) => address,
            Err(_) => {
                return (
                    report,
                    Err("the account name makes no valid address".into()),
                )
            }
        };
        if let Err(err) = self
            .submit(settings, message, vec![address], "deliverability")
            .await
        {
            return (
                report,
                Err(format!(
                    "the mail server did not accept the test message ({})",
                    err.message
                )),
            );
        }
        let mut last = Ok(Some(MailTesterAnswer::Waiting));
        for _ in 0..MAIL_TESTER_POLLS {
            self.checkers.pause(MAIL_TESTER_POLL_GAP).await;
            last = match self.checkers.mail_tester(&id).await {
                Ok(body) => parse_mail_tester(&body).map(Some),
                Err(why) => Err(why),
            };
            if !matches!(last, Ok(Some(MailTesterAnswer::Waiting))) {
                break;
            }
        }
        (report, last)
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
        self.submit(
            settings,
            &params.message,
            recipients,
            &params.destination.destination_id,
        )
        .await?;
        Ok(NotifySent { provider_id: None })
    }

    /// Runs Postmark's spam check on the composed sample, reads the From
    /// domain's SPF and DMARC records, and, when an account is set, sends
    /// the sample through the server to mail-tester and reads its result.
    async fn check_deliverability(
        &self,
        params: NotifyDeliverabilityParams,
    ) -> Result<NotifyDeliverabilityReport, PluginError> {
        let settings = self.settings()?;
        let mut checks = Vec::new();
        let mut advice: Vec<NotifyDeliverabilityAdvice> = Vec::new();

        let sample = self.compose_for(
            settings,
            &params.message,
            std::slice::from_ref(&settings.from),
            "deliverability",
        );
        let spam = match self.checkers.spamcheck(&sample).await {
            Ok(body) => parse_spamcheck(&body),
            Err(why) => Err(why),
        };
        let (check, found) = postmark_findings(spam);
        checks.push(check);
        advice.extend(found);

        let domain = settings
            .from
            .rsplit_once('@')
            .map_or("", |(_, domain)| domain)
            .to_ascii_lowercase();
        let spf = match self.checkers.txt(&domain).await {
            Ok(body) => parse_doh_txt(&body),
            Err(why) => Err(why),
        };
        let dmarc = match self.checkers.txt(&format!("_dmarc.{domain}")).await {
            Ok(body) => parse_doh_txt(&body),
            Err(why) => Err(why),
        };
        let (check, found) = dns_findings(&domain, spf, dmarc);
        checks.push(check);
        advice.extend(found);

        let (report, answer) = match settings.mail_tester_account.as_deref() {
            Some(account) => {
                self.mail_tester_run(settings, &params.message, account)
                    .await
            }
            None => (String::new(), Ok(None)),
        };
        if let Err(why) = &answer {
            if why.starts_with("the mail server") {
                advice.push(NotifyDeliverabilityAdvice {
                    outcome: NotifyCheckOutcome::Failed,
                    title: "Fix the mail server settings".to_owned(),
                    detail: format!(
                        "The server refused the test message: {why}. Check the server, connection security, user name, password and From address."
                    ),
                    sources: vec![MAIL_TESTER.to_owned()],
                });
            }
        }
        let (check, found) = mail_tester_findings(answer, &report);
        checks.push(check);
        advice.extend(found);

        Ok(NotifyDeliverabilityReport {
            checks,
            advice: merge_advice(advice),
        })
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
