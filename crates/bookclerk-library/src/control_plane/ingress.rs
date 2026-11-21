//! The library's public address from an ingress plugin (see
//! `docs/adr/ingress-plugins.md`).
//!
//! One library document, `core.ingress`, holds which plugin instance is the
//! library's active ingress, what it exposes, and the public origin it
//! reported. A library has at most one active ingress, so it has exactly one
//! public origin.
//!
//! The first origin the active instance reports is adopted. A later report
//! naming a different host is only recorded: changing the library's name
//! breaks passkeys and sign-in redirects, so the operator accepts it
//! explicitly ([`accept_reported_origin`]).

use serde::{Deserialize, Serialize};

use bookclerk_plugin_abi::origin::{parse_ingress_origin, IngressOrigin};
use bookclerk_plugin_abi::IngressStarted;

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument, PluginInstanceId};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace of the library document that holds the ingress settings.
pub const INGRESS_NAMESPACE: &str = "core.ingress";

/// Schema version of [`IngressSettings`].
pub const INGRESS_SCHEMA_VERSION: i64 = 1;

/// What public traffic from the ingress may reach.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum IngressExposure {
    /// Only what branches and clerks need: the node API, sign-in and the
    /// clerk routes. The default, so turning ingress on never publishes the
    /// web app by surprise.
    #[default]
    Branches,
    /// The whole library, web app included.
    Library,
}

impl IngressExposure {
    /// Stable spelling used by the CLI and the API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branches => "branches",
            Self::Library => "library",
        }
    }
}

/// The `core.ingress` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressSettings {
    /// The plugin instance serving the library's public address, as its
    /// hyphenated UUID; `None` when ingress is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_instance: Option<String>,
    /// What public traffic may reach.
    #[serde(default)]
    pub exposure: IngressExposure,
    /// The adopted public origin, `https://<host>`; `None` until the active
    /// instance first reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The last origin the active instance reported when it differs from
    /// [`IngressSettings::origin`]; waits for the operator to accept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_origin: Option<String>,
}

impl IngressSettings {
    /// The active instance, parsed.
    ///
    /// # Returns
    ///
    /// `None` when ingress is off or the stored id does not parse (a
    /// document written by hand); either way no instance is active.
    #[must_use]
    pub fn active(&self) -> Option<PluginInstanceId> {
        self.active_instance
            .as_deref()
            .and_then(|raw| PluginInstanceId::parse(raw).ok())
    }

    /// The public origin to serve while ingress is on: the adopted origin of
    /// the active instance, `None` when ingress is off.
    #[must_use]
    pub fn serving_origin(&self) -> Option<&str> {
        self.active().and(self.origin.as_deref())
    }
}

/// What [`record_started`] did with a reported origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginReport {
    /// The origin is the library's public origin now (first report, or the
    /// same host again).
    Adopted(IngressOrigin),
    /// The origin differs from the adopted one; it is recorded and waits for
    /// the operator.
    Pending {
        /// The origin the library keeps serving under.
        adopted: String,
        /// The origin the plugin reported.
        reported: IngressOrigin,
    },
    /// The reporting instance is not the active one; nothing was written.
    NotActive,
}

/// Loads the `core.ingress` document.
///
/// # Returns
///
/// The stored document, or `None` when nobody has written one: ingress is
/// then off.
///
/// # Errors
///
/// Returns the read error, or an error when the stored document is invalid
/// or of an unknown schema.
pub async fn load_ingress_settings(
    store: &LibraryStore,
) -> Result<Option<ConfigurationDocument<IngressSettings>>> {
    load_document(store, &DocumentKey::library(INGRESS_NAMESPACE))
        .await?
        .map(parse)
        .transpose()
}

/// The ingress settings: the stored document, or ingress off.
///
/// # Errors
///
/// Returns the read or parse error.
pub async fn ingress_settings(store: &LibraryStore) -> Result<IngressSettings> {
    Ok(load_ingress_settings(store)
        .await?
        .map(|document| document.body)
        .unwrap_or_default())
}

/// Makes `instance` the library's active ingress, replacing any other.
///
/// Switching to another instance keeps the adopted origin: the new instance's
/// first report then has to match it or wait for the operator, like any
/// change of name.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - The operator or a verified Owner.
/// * `instance` - The plugin instance to activate.
///
/// # Errors
///
/// Returns an error when the actor may not write, the store refuses the
/// write, or the document keeps changing underneath.
pub async fn activate_ingress(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance: &PluginInstanceId,
) -> Result<ConfigurationDocument<IngressSettings>> {
    let id = instance.as_str().to_string();
    write(store, actor, move |mut settings| {
        if settings.active_instance.as_deref() != Some(id.as_str()) {
            settings.active_instance = Some(id.clone());
            settings.reported_origin = None;
        }
        settings
    })
    .await
}

/// Turns ingress off. The adopted origin is kept, so turning the same
/// provider back on serves under the same name.
///
/// # Errors
///
/// Returns an error when the actor may not write or the write fails.
pub async fn deactivate_ingress(
    store: &LibraryStore,
    actor: &ConfigActor,
) -> Result<ConfigurationDocument<IngressSettings>> {
    write(store, actor, |mut settings| {
        settings.active_instance = None;
        settings.reported_origin = None;
        settings
    })
    .await
}

/// Sets what public traffic may reach.
///
/// # Errors
///
/// Returns an error when the actor may not write or the write fails.
pub async fn set_ingress_exposure(
    store: &LibraryStore,
    actor: &ConfigActor,
    exposure: IngressExposure,
) -> Result<ConfigurationDocument<IngressSettings>> {
    write(store, actor, move |mut settings| {
        settings.exposure = exposure;
        settings
    })
    .await
}

/// Adopts the origin the active instance reported and is waiting on the
/// operator.
///
/// # Returns
///
/// The document after the change; unchanged when nothing was pending.
///
/// # Errors
///
/// Returns an error when the actor may not write or the write fails.
pub async fn accept_reported_origin(
    store: &LibraryStore,
    actor: &ConfigActor,
) -> Result<ConfigurationDocument<IngressSettings>> {
    write(store, actor, |mut settings| {
        if let Some(reported) = settings.reported_origin.take() {
            settings.origin = Some(reported);
        }
        settings
    })
    .await
}

/// Records what `instance` reported from `Ingress.start`.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - The host writing on the plugin's behalf.
/// * `instance` - The instance that started.
/// * `started` - Its report, checked here with [`parse_ingress_origin`].
///
/// # Returns
///
/// Whether the origin was adopted, is waiting for the operator, or came from
/// an instance that is not active.
///
/// # Errors
///
/// [`LibraryError::Invalid`] when the origin is not `https://` and a DNS
/// name, or the write error.
pub async fn record_started(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance: &PluginInstanceId,
    started: &IngressStarted,
) -> Result<OriginReport> {
    let origin = parse_ingress_origin(started)
        .map_err(|err| LibraryError::Invalid(format!("ingress origin: {}", err.message)))?;
    let current = ingress_settings(store).await?;
    let Some(next) = report_into(&current, instance, &origin) else {
        return Ok(OriginReport::NotActive);
    };
    let outcome = classify(&next, origin);
    if next != current {
        let id = instance.as_str().to_string();
        let reported = next.clone();
        write(store, actor, move |settings| {
            // Only the instance that is still active may write.
            if settings.active_instance.as_deref() == Some(id.as_str()) {
                reported.clone()
            } else {
                settings
            }
        })
        .await?;
    }
    Ok(outcome)
}

/// The settings after `instance` reported `origin`, or `None` when it is not
/// the active instance. Pure.
fn report_into(
    settings: &IngressSettings,
    instance: &PluginInstanceId,
    origin: &IngressOrigin,
) -> Option<IngressSettings> {
    if settings.active().as_ref() != Some(instance) {
        return None;
    }
    let mut next = settings.clone();
    match next.origin.as_deref() {
        None => {
            next.origin = Some(origin.as_str().to_string());
            next.reported_origin = None;
        }
        Some(adopted) if adopted == origin.as_str() => next.reported_origin = None,
        Some(_) => next.reported_origin = Some(origin.as_str().to_string()),
    }
    Some(next)
}

/// What the settings after a report mean for `origin`.
fn classify(next: &IngressSettings, origin: IngressOrigin) -> OriginReport {
    match next.origin.as_deref() {
        Some(adopted) if adopted != origin.as_str() => OriginReport::Pending {
            adopted: adopted.to_string(),
            reported: origin,
        },
        _ => OriginReport::Adopted(origin),
    }
}

/// Writes the settings that `next` makes of the stored ones, under
/// compare-and-swap, creating the document when absent and retrying a few
/// times on a concurrent edit.
async fn write(
    store: &LibraryStore,
    actor: &ConfigActor,
    next: impl Fn(IngressSettings) -> IngressSettings,
) -> Result<ConfigurationDocument<IngressSettings>> {
    let key = DocumentKey::library(INGRESS_NAMESPACE);
    for attempt in 0..4 {
        let current = match load_ingress_settings(store).await? {
            Some(document) => document,
            None => parse(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    INGRESS_SCHEMA_VERSION,
                    &to_json(&IngressSettings::default())?,
                    "core-ingress-default",
                )
                .await?,
            )?,
        };
        let settings = next(current.body.clone());
        if current.body == settings {
            return Ok(current);
        }
        let operation_id = format!(
            "core-ingress-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            INGRESS_SCHEMA_VERSION,
            current.revision,
            &to_json(&settings)?,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict("core.ingress kept changing; retry".into()))
}

/// Parses a stored `core.ingress` document.
fn parse(stored: StoredDocument) -> Result<ConfigurationDocument<IngressSettings>> {
    if stored.schema_version != INGRESS_SCHEMA_VERSION {
        return Err(documents::unsupported(
            INGRESS_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: IngressSettings = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Invalid(format!("invalid ingress settings: {err}")))?;
    Ok(document(stored, body))
}

/// Serializes the settings for storage.
fn to_json(settings: &IngressSettings) -> Result<String> {
    serde_json::to_string(settings)
        .map_err(|err| LibraryError::Invalid(format!("invalid ingress settings: {err}")))
}

#[cfg(test)]
#[path = "ingress_tests.rs"]
mod tests;
