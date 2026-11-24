//! The notifications web API (`docs/server/notifications.md`).
//!
//! Administrator routes (`/api/notifications/*`, the operator or an Owner
//! or Administrator) list what can be sent and through what, manage the
//! library's channels, decide which types reach which channels and which
//! notifiers patrons may pick, and read the delivery log. Patron routes
//! (`/api/me/notifications/*`, any signed-in user, for themselves) manage
//! their own destinations, choose what reaches them, and read their inbox.
//!
//! The rules live in [`bookclerk_library::notifications`]; these handlers
//! only check who may call and translate the bodies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use bookclerk_library::notifications::{
    host_notification_type, qualified_plugin_type, DeliveryRecord, Destination, DestinationKind,
    InboxEntry, NewDestination, NewNotification, PatronChoice, PatronOffer, Route,
    HOST_NOTIFICATION_TYPES, TEST_NOTIFICATION_TYPE,
};
use bookclerk_library::{LibraryError, LibraryStore, NotifyCursor};
use bookclerk_plugin_abi::notifications::{
    check_address_against_fields, check_validated, is_qualified_notification_type,
    parse_address_document, AddressFieldRule, NotifyAddress,
};
use bookclerk_plugin_abi::{
    NotificationAlertAction, NotificationAudience, NotificationField, NotificationSeverity,
    NotifyValidateParams, NotifyValidated,
};
use bookclerk_plugin_manifest::NotifierDestination;
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::auth::UserCaller;
use crate::http_error::ErrorMessage;

/// An error answer: a status and a JSON body, or the session-check refusal.
type ApiError = axum::response::ErrorResponse;

/// Default page size of the delivery log and the inbox.
const DEFAULT_PAGE: u64 = 50;

/// A `400` with `message`.
fn bad(message: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message))).into()
}

/// A `404` with `message`.
fn missing(message: impl std::fmt::Display) -> ApiError {
    (StatusCode::NOT_FOUND, Json(ErrorMessage::new(message))).into()
}

/// Longest a route waits for a notifier to check an address, queueing
/// included: under the 8-second limit every route has.
const VALIDATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(7);

/// Maps a store error: refusals keep their words, the rest is logged and
/// answered as a 500.
fn store_error(route: &str, err: LibraryError) -> ApiError {
    match err {
        LibraryError::Invalid(message) => bad(message),
        LibraryError::NotFound(_) => missing(err),
        other => {
            tracing::error!(error = %other, route, "notifications call failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new(
                    "the library could not be read or changed",
                )),
            )
                .into()
        }
    }
}

/// Admits the operator, an Owner or an Administrator; a Member (even one
/// who may acquire) is refused. No administrator switch gates
/// notifications: every Administrator manages them.
///
/// # Errors
///
/// 401 when no one is signed in, 403 for anyone else.
async fn administrator(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    crate::auth::library_administrator(state, headers, None)
        .await
        .map(|_| ())
        .map_err(|refusal| {
            refusal
                .answer(|status| {
                    let message = if status == StatusCode::UNAUTHORIZED {
                        "sign in first"
                    } else {
                        "only an Administrator can manage notifications"
                    };
                    (status, Json(ErrorMessage::new(message)))
                })
                .into()
        })
}

/// The library user the route's sign-in check admitted, or a `403` for the
/// operator (the operator token alone has no inbox). The identity is the
/// one the middleware already resolved, so a session lookup that fails
/// keeps that check's retryable answer rather than becoming a `403` here.
fn patron(caller: &UserCaller) -> Result<i64, ApiError> {
    match caller {
        UserCaller::User { user_id } => Ok(*user_id),
        UserCaller::Operator => Err(ApiError::from((
            StatusCode::FORBIDDEN,
            Json(ErrorMessage::new(
                "this session has no library user to keep notification settings for",
            )),
        ))),
    }
}

/// A severity from its wire name.
fn parse_severity(s: &str) -> Result<NotificationSeverity, ApiError> {
    NotificationSeverity::from_wire_name(s)
        .ok_or_else(|| bad(format!("severity `{s}` is not info, warning or critical")))
}

/// A page cursor from its token.
fn parse_cursor(token: Option<&str>) -> Result<Option<NotifyCursor>, ApiError> {
    token
        .filter(|t| !t.is_empty())
        .map(NotifyCursor::decode)
        .transpose()
        .map_err(|err| bad(err.to_string()))
}

// --- What can be sent, and through what -----------------------------------

/// One notification type a routing rule or a patron choice can name.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct NotificationTypeJson {
    /// Qualified id: a host type (`test`) or `<plugin id>:<type>`.
    #[schema(example = "ntfy_relay:sync_failed")]
    id: String,
    /// Who it is for: `operational` (routed to channels) or `patron`
    /// (offered to patrons).
    #[schema(example = "operational")]
    audience: String,
    /// Default severity: `info`, `warning` or `critical`.
    #[schema(example = "warning")]
    severity: String,
    /// Title shown in the routing matrix and on each notification.
    #[schema(example = "Sync failed")]
    title: String,
    /// The plugin that raises it; empty for a host type.
    #[schema(example = "ntfy_relay")]
    source: String,
}

/// One field a patron fills in for a personal destination.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct AddressFieldJson {
    /// Key in the address document.
    #[schema(example = "chat_id")]
    key: String,
    /// Label shown on the form.
    #[schema(example = "Chat id")]
    label: String,
    /// Whether a destination must carry it.
    #[schema(example = true)]
    required: bool,
    /// Whether the value is a secret: shown masked, never logged.
    #[schema(example = false)]
    secret: bool,
}

/// One notifier instance.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct NotifierJson {
    /// Plugin instance id.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// Canonical plugin key.
    #[schema(example = "plugin:telegram")]
    plugin_key: String,
    /// Name shown in pickers.
    #[schema(example = "Telegram")]
    name: String,
    /// Whether administrators can make library channels on it.
    #[schema(example = false)]
    channel: bool,
    /// Whether patrons can add their own address on it.
    #[schema(example = true)]
    personal: bool,
    /// What a patron enters for a personal destination.
    address_fields: Vec<AddressFieldJson>,
    /// Whether it can send account mail (invites, email verification and
    /// password resets).
    #[schema(example = false)]
    account_mail: bool,
}

/// Everything the routing screens pick from.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct NotificationCatalog {
    /// Host types first, then plugin types by id.
    types: Vec<NotificationTypeJson>,
    /// Notifier instances installed on this server.
    notifiers: Vec<NotifierJson>,
}

impl NotificationCatalog {
    /// The type `id`, when it is known.
    fn notification_type(&self, id: &str) -> Option<&NotificationTypeJson> {
        self.types.iter().find(|t| t.id == id)
    }

    /// The type `id`, when it is known and patrons choose it.
    ///
    /// A stored offer for a type that is gone, or that a plugin update
    /// made operational, makes no notifier eligible for patrons.
    fn patron_type(&self, id: &str) -> Option<&NotificationTypeJson> {
        self.notification_type(id)
            .filter(|t| t.audience == NotificationAudience::Patron.wire_name())
    }

    /// The notifier instance `id`, when it is installed.
    fn notifier(&self, id: &str) -> Option<&NotifierJson> {
        self.notifiers.iter().find(|n| n.plugin_instance_id == id)
    }
}

/// The catalog: host types, the types and notifiers of the plugins
/// installed here, and the instances of those notifiers.
///
/// # Errors
///
/// A 500 when the instances cannot be read; a failed plugin discovery
/// leaves only the host types.
async fn catalog(
    state: &AppState,
    library: &LibraryStore,
) -> Result<NotificationCatalog, ApiError> {
    let mut types: Vec<NotificationTypeJson> = HOST_NOTIFICATION_TYPES
        .iter()
        .map(|t| NotificationTypeJson {
            id: t.id.to_string(),
            audience: t.audience.wire_name().to_string(),
            severity: t.severity.wire_name().to_string(),
            title: t.title.to_string(),
            source: String::new(),
        })
        .collect();
    let config = state.config.read().await.clone();
    let discovered = if config.paths.is_none() {
        Vec::new()
    } else {
        tokio::task::spawn_blocking(move || bookclerk_plugin_host::discover_plugins(&config))
            .await
            .ok()
            .and_then(|found| {
                found
                    .map_err(|err| tracing::warn!(error = %err, "plugin discovery failed"))
                    .ok()
            })
            .unwrap_or_default()
    };
    let instances = bookclerk_library::control_plane::list_all_plugin_instances(library)
        .await
        .map_err(|err| store_error("notification_catalog", err))?;
    let mut plugin_types = Vec::new();
    let mut notifiers = Vec::new();
    for plugin in &discovered {
        let manifest = &plugin.manifest;
        for t in &manifest.notifications.types {
            plugin_types.push(NotificationTypeJson {
                id: qualified_plugin_type(&manifest.id, &t.id),
                audience: t.audience.wire_name().to_string(),
                severity: t.severity.wire_name().to_string(),
                title: t.title.clone(),
                source: manifest.id.clone(),
            });
        }
        let Some(notifier) = &manifest.notifier else {
            continue;
        };
        let key = plugin.plugin_key().canonical();
        for instance in instances.iter().filter(|i| i.plugin_key == key) {
            notifiers.push(NotifierJson {
                plugin_instance_id: instance.id.as_str().to_string(),
                plugin_key: key.to_string(),
                name: if instance.display_name.is_empty() {
                    manifest.name.clone().unwrap_or_else(|| manifest.id.clone())
                } else {
                    instance.display_name.clone()
                },
                channel: notifier.sends_to(NotifierDestination::Channel),
                personal: notifier.sends_to(NotifierDestination::Personal),
                address_fields: notifier
                    .address_fields
                    .iter()
                    .map(|f| AddressFieldJson {
                        key: f.key.clone(),
                        label: f.label.clone(),
                        required: f.required,
                        secret: f.secret,
                    })
                    .collect(),
                account_mail: notifier.account_mail,
            });
        }
    }
    plugin_types.sort_by(|a, b| a.id.cmp(&b.id));
    plugin_types.dedup_by(|a, b| a.id == b.id);
    types.extend(plugin_types);
    notifiers.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.plugin_instance_id.cmp(&b.plugin_instance_id))
    });
    Ok(NotificationCatalog { types, notifiers })
}

/// Lists the notification types and the notifier instances the routing
/// screens pick from.
///
/// Types come from the host and from the `[[notifications.types]]` of the
/// plugins installed on this server; notifiers are the instances of
/// installed `notifier` plugins.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    get,
    path = "/api/notifications/catalog",
    responses(
        (status = 200, description = "The types and notifiers.", body = NotificationCatalog),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn get_catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<NotificationCatalog>, ApiError> {
    administrator(&state, &headers).await?;
    let library = state.library_snapshot().await;
    Ok(Json(catalog(&state, &library).await?))
}

/// Whether `plugin_instance_id` is an installed notifier whose plugin sends
/// account mail.
///
/// # Errors
///
/// A 500 when the instances cannot be read.
pub(crate) async fn is_account_mail_notifier(
    state: &AppState,
    library: &LibraryStore,
    plugin_instance_id: &str,
) -> Result<bool, ApiError> {
    Ok(catalog(state, library)
        .await?
        .notifiers
        .iter()
        .any(|n| n.account_mail && n.plugin_instance_id == plugin_instance_id))
}

// --- Channels -------------------------------------------------------------

/// A destination as administrators and its owner see it. The address is
/// never returned: it may hold secrets.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct DestinationJson {
    /// Destination id.
    #[schema(example = "8a7b6c5d-4e3f-4a2b-9c1d-0e9f8a7b6c5d")]
    id: String,
    /// `channel` (the library's) or `personal` (a patron's).
    kind: DestinationKind,
    /// The notifier instance that carries it.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// Name shown in pickers.
    #[schema(example = "Ops room")]
    label: String,
    /// Deliveries in a row that ended `failed`, whether refused for good
    /// or out of attempts; a delivery, or resuming the destination, resets
    /// it to 0. It pauses at 5.
    #[schema(example = 0)]
    failure_count: i64,
    /// When repeated failures paused it (RFC 3339); `null` while active.
    #[schema(example = "2026-11-21T10:00:00+00:00")]
    paused_at: Option<String>,
    /// Why it was paused.
    #[schema(example = "unauthenticated: the webhook was deleted")]
    paused_reason: Option<String>,
    /// When it was made (RFC 3339).
    #[schema(example = "2026-11-20T09:00:00+00:00")]
    created_at: String,
}

impl From<Destination> for DestinationJson {
    fn from(d: Destination) -> Self {
        Self {
            id: d.id,
            kind: d.kind,
            plugin_instance_id: d.plugin_instance_id,
            label: d.label,
            failure_count: d.failure_count,
            paused_at: d.paused_at,
            paused_reason: d.paused_reason,
            created_at: d.created_at,
        }
    }
}

/// The library's channels.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ChannelList {
    /// Oldest first.
    channels: Vec<DestinationJson>,
}

/// Body of `POST /api/notifications/channels`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewChannelBody {
    /// A notifier instance that sends to channels; its own settings say
    /// where (a webhook URL, a service key).
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// Name shown in the routing matrix, 1 to 128 bytes on one line.
    #[schema(example = "Ops room")]
    label: String,
}

/// Body of `PATCH /api/notifications/channels/{id}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenameBody {
    /// The new name, 1 to 128 bytes on one line.
    #[schema(example = "Ops room")]
    label: String,
}

/// The library a write goes to, with a shared job-runtime permit that
/// holds off a database reload until the write is done, so a write never
/// lands on (and reports success from) a superseded store.
///
/// # Errors
///
/// A 500 when a reload holds the runtime for longer than the permit waits.
async fn write_snapshot<'a>(
    state: &'a AppState,
    operation: &'static str,
) -> Result<(tokio::sync::RwLockReadGuard<'a, ()>, LibraryStore), ApiError> {
    let permit = crate::jobs::admission_permit(state)
        .await
        .map_err(|err| store_error(operation, LibraryError::Other(err)))?;
    Ok((permit, state.library_snapshot().await))
}

/// The channel `id`, or a 404 for a missing or personal destination.
async fn channel(library: &LibraryStore, id: &str) -> Result<Destination, ApiError> {
    library
        .notify_destination(id)
        .await
        .map_err(|err| store_error("channel", err))?
        .filter(|d| d.kind == DestinationKind::Channel)
        .ok_or_else(|| missing(format!("channel {id} not found")))
}

/// Lists the library's channels.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    get,
    path = "/api/notifications/channels",
    responses(
        (status = 200, description = "The channels.", body = ChannelList),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn list_channels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ChannelList>, ApiError> {
    administrator(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let channels = library
        .notify_channels()
        .await
        .map_err(|err| store_error("list_channels", err))?;
    Ok(Json(ChannelList {
        channels: channels.into_iter().map(DestinationJson::from).collect(),
    }))
}

/// Makes a library channel on a notifier instance.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    post,
    path = "/api/notifications/channels",
    request_body = NewChannelBody,
    responses(
        (status = 201, description = "The channel.", body = DestinationJson),
        (status = 400, description = "A bad label, or an instance that is not a channel notifier.", body = ErrorMessage, example = json!({"error": "plugin instance 3f9c2b7e does not send to channels"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The channel could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<NewChannelBody>,
) -> Result<(StatusCode, Json<DestinationJson>), ApiError> {
    administrator(&state, &headers).await?;
    let (_permit, library) = write_snapshot(&state, "post_channel").await?;
    let catalog = catalog(&state, &library).await?;
    if !catalog
        .notifier(&body.plugin_instance_id)
        .is_some_and(|n| n.channel)
    {
        return Err(bad(format!(
            "plugin instance {} does not send to channels",
            body.plugin_instance_id
        )));
    }
    let created = library
        .create_notify_destination(&NewDestination {
            plugin_instance_id: body.plugin_instance_id,
            kind: DestinationKind::Channel,
            user_id: None,
            label: body.label,
            address_json: String::new(),
        })
        .await
        .map_err(|err| store_error("post_channel", err))?;
    tracing::info!(destination = %created.id, "notification channel created");
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// Renames a channel.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    patch,
    path = "/api/notifications/channels/{id}",
    params(("id" = String, Path, description = "Channel id.")),
    request_body = RenameBody,
    responses(
        (status = 200, description = "The channel.", body = DestinationJson),
        (status = 400, description = "A bad label.", body = ErrorMessage, example = json!({"error": "a destination label is one line of 1 to 128 bytes"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 404, description = "No such channel.", body = ErrorMessage, example = json!({"error": "channel 8a7b6c5d not found"})),
        (status = 500, description = "The channel could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn patch_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RenameBody>,
) -> Result<Json<DestinationJson>, ApiError> {
    administrator(&state, &headers).await?;
    let (_permit, library) = write_snapshot(&state, "patch_channel").await?;
    channel(&library, &id).await?;
    library
        .rename_notify_destination(&id, &body.label)
        .await
        .map_err(|err| store_error("patch_channel", err))?;
    Ok(Json(channel(&library, &id).await?.into()))
}

/// Deletes a channel with its routes and delivery-log rows.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    delete,
    path = "/api/notifications/channels/{id}",
    params(("id" = String, Path, description = "Channel id.")),
    responses(
        (status = 204, description = "The channel is gone."),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 404, description = "No such channel.", body = ErrorMessage, example = json!({"error": "channel 8a7b6c5d not found"})),
        (status = 500, description = "The channel could not be deleted.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn delete_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    administrator(&state, &headers).await?;
    let (_permit, library) = write_snapshot(&state, "delete_channel").await?;
    channel(&library, &id).await?;
    library
        .delete_notify_destination(&id)
        .await
        .map_err(|err| store_error("delete_channel", err))?;
    tracing::info!(destination = %id, "notification channel deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Resumes a channel repeated failures paused, and clears its failure
/// count.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    post,
    path = "/api/notifications/channels/{id}/resume",
    params(("id" = String, Path, description = "Channel id.")),
    responses(
        (status = 200, description = "The channel.", body = DestinationJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 404, description = "No such channel.", body = ErrorMessage, example = json!({"error": "channel 8a7b6c5d not found"})),
        (status = 500, description = "The channel could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_channel_resume(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DestinationJson>, ApiError> {
    administrator(&state, &headers).await?;
    let (_permit, library) = write_snapshot(&state, "post_channel_resume").await?;
    channel(&library, &id).await?;
    library
        .resume_notify_destination(&id)
        .await
        .map_err(|err| store_error("post_channel_resume", err))?;
    Ok(Json(channel(&library, &id).await?.into()))
}

/// The answer to a test send.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct TestSent {
    /// The stored test notification.
    #[schema(example = "1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e")]
    notification_id: String,
    /// Its delivery; follow it in the delivery log.
    #[schema(example = "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b")]
    delivery_id: String,
}

/// Sends a test notification to one channel, whatever the routes say.
///
/// The send runs as a job; its outcome lands in the delivery log.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    post,
    path = "/api/notifications/channels/{id}/test",
    params(("id" = String, Path, description = "Channel id.")),
    responses(
        (status = 202, description = "The test is accepted. Its send job is queued, or, when the queue is full or enqueueing fails, may be queued by a later sweep.", body = TestSent),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 404, description = "No such channel.", body = ErrorMessage, example = json!({"error": "channel 8a7b6c5d not found"})),
        (status = 409, description = "The channel is paused; resume it first.", body = ErrorMessage, example = json!({"error": "the channel is paused; resume it before testing it"})),
        (status = 500, description = "The test could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_channel_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<(StatusCode, Json<TestSent>), ApiError> {
    administrator(&state, &headers).await?;
    // Hold job admission from the snapshot through routing, so a database
    // reload cannot leave the test's job on a superseded store.
    let (_permit, library) = write_snapshot(&state, "post_channel_test").await?;
    let destination = channel(&library, &id).await?;
    if destination.paused_at.is_some() {
        // A paused channel's deliveries are dropped, so a test would say
        // nothing about whether it works.
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorMessage::new(
                "the channel is paused; resume it before testing it",
            )),
        )
            .into());
    }
    let host = host_notification_type(TEST_NOTIFICATION_TYPE).ok_or_else(|| {
        store_error(
            "post_channel_test",
            LibraryError::Invalid("the host test type is missing".into()),
        )
    })?;
    let notification = NewNotification {
        notification_type: host.id.to_string(),
        audience: host.audience,
        severity: host.severity,
        title: host.title.to_string(),
        text: format!(
            "A test from Bookclerk to {}. If you can read this, the channel works.",
            destination.label
        ),
        // No settings screen in the web app links here yet.
        link_path: None,
        fields: Vec::new(),
        group_key: None,
        alert_action: NotificationAlertAction::None,
        alert_key: None,
        source: String::new(),
        patron: None,
    };
    let limits = crate::notify_jobs::notify_queue_limits(&state).await;
    let routed = library
        .route_notification_to(&notification, &id, limits)
        .await
        .map_err(|err| store_error("post_channel_test", err))?;
    let delivery_id = routed
        .deliveries
        .first()
        .map(|d| d.id.clone())
        .unwrap_or_default();
    Ok((
        StatusCode::ACCEPTED,
        Json(TestSent {
            notification_id: routed.notification_id,
            delivery_id,
        }),
    ))
}

// --- Routes and patron offers ----------------------------------------------

/// One route: a channel, or the built-in "In app" target, and the least
/// severity it takes.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouteJson {
    /// The channel's id, or `in-app` for the built-in target that puts the
    /// notification in every active Owner's and Administrator's inbox
    /// (the bell in the web app, and browser or desktop notifications where
    /// they turned them on). `in-app` runs no plugin and is listed first.
    #[schema(example = "8a7b6c5d-4e3f-4a2b-9c1d-0e9f8a7b6c5d")]
    destination_id: String,
    /// `info`, `warning` or `critical`: notifications less urgent than
    /// this skip the channel.
    #[schema(example = "warning")]
    min_severity: String,
}

/// Every routing rule, by type.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RouteTable {
    /// Type id to its routes.
    #[schema(example = json!({"test": [{"destination_id": "8a7b6c5d-4e3f-4a2b-9c1d-0e9f8a7b6c5d", "min_severity": "info"}]}))]
    routes: BTreeMap<String, Vec<RouteJson>>,
}

/// Body of `PUT /api/notifications/routes/{type}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoutesBody {
    /// The type's routes, replacing the old ones; at most 32. Empty sends
    /// the type nowhere.
    #[schema(max_items = 32)]
    routes: Vec<RouteJson>,
}

/// The known operational type `id`, or a 400.
fn known_type<'a>(
    catalog: &'a NotificationCatalog,
    id: &str,
    audience: NotificationAudience,
) -> Result<&'a NotificationTypeJson, ApiError> {
    if !is_qualified_notification_type(id) {
        return Err(bad(format!("`{id}` is not a notification type id")));
    }
    let found = catalog
        .notification_type(id)
        .ok_or_else(|| bad(format!("notification type `{id}` is not known here")))?;
    if found.audience != audience.wire_name() {
        return Err(bad(format!(
            "notification type `{id}` is for {}, not {}",
            found.audience,
            audience.wire_name()
        )));
    }
    Ok(found)
}

/// Checks the type a routes or offers replacement names. Clearing (an
/// empty replacement) also takes a type the catalog no longer knows, or
/// one now of the other audience, such as one a plugin upgrade dropped or
/// moved, so its stale rules can be removed; anything else must be a known
/// type of `audience`.
///
/// # Errors
///
/// A 400 as [`known_type`] answers, except when clearing a type that is
/// unknown or of the other audience.
fn replaceable_type(
    catalog: &NotificationCatalog,
    id: &str,
    audience: NotificationAudience,
    clearing: bool,
) -> Result<(), ApiError> {
    // Rows left behind when a type was removed, or when a plugin update
    // moved it to the other audience, can always be cleared.
    if clearing
        && is_qualified_notification_type(id)
        && catalog
            .notification_type(id)
            .is_none_or(|known| known.audience != audience.wire_name())
    {
        return Ok(());
    }
    known_type(catalog, id, audience).map(|_| ())
}

/// Lists every routing rule.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    get,
    path = "/api/notifications/routes",
    responses(
        (status = 200, description = "The routes, by type.", body = RouteTable),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn get_routes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<RouteTable>, ApiError> {
    administrator(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let routes = library
        .notify_routes()
        .await
        .map_err(|err| store_error("get_routes", err))?;
    Ok(Json(RouteTable {
        routes: routes
            .into_iter()
            .map(|(t, rs)| {
                (
                    t,
                    rs.into_iter()
                        .map(|r| RouteJson {
                            destination_id: r.destination_id,
                            min_severity: r.min_severity.wire_name().to_string(),
                        })
                        .collect(),
                )
            })
            .collect(),
    }))
}

/// Replaces the routes of one operational type.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    put,
    path = "/api/notifications/routes/{type}",
    params(("type" = String, Path, description = "Notification type id.")),
    request_body = RoutesBody,
    responses(
        (status = 204, description = "The routes are replaced."),
        (status = 400, description = "An unknown or patron type (either may still be cleared with no routes, to remove rules left from before the type changed), a bad severity, a personal destination, a repeat, or more than 32.", body = ErrorMessage, example = json!({"error": "destination 8a7b6c5d is routed twice"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 404, description = "A destination does not exist.", body = ErrorMessage, example = json!({"error": "notification destination 8a7b6c5d not found"})),
        (status = 500, description = "The routes could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn put_routes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(notification_type): AxumPath<String>,
    Json(body): Json<RoutesBody>,
) -> Result<StatusCode, ApiError> {
    administrator(&state, &headers).await?;
    let (_permit, library) = write_snapshot(&state, "put_routes").await?;
    let catalog = catalog(&state, &library).await?;
    replaceable_type(
        &catalog,
        &notification_type,
        NotificationAudience::Operational,
        body.routes.is_empty(),
    )?;
    let routes = body
        .routes
        .iter()
        .map(|r| {
            Ok(Route {
                destination_id: r.destination_id.clone(),
                min_severity: parse_severity(&r.min_severity)?,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    library
        .set_notify_routes(&notification_type, &routes)
        .await
        .map_err(|err| store_error("put_routes", err))?;
    tracing::info!(notification_type = %notification_type, routes = routes.len(), "notification routes set");
    Ok(StatusCode::NO_CONTENT)
}

/// One notifier offered to patrons for a type.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfferJson {
    /// A notifier instance that sends to personal destinations.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// Whether a patron who chose nothing gets the type there.
    #[schema(example = true)]
    default_on: bool,
}

/// Every patron offer, by type.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct OfferTable {
    /// Type id to its offers.
    #[schema(example = json!({"discover:new_book": [{"plugin_instance_id": "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b", "default_on": true}]}))]
    offers: BTreeMap<String, Vec<OfferJson>>,
}

/// Body of `PUT /api/notifications/offers/{type}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OffersBody {
    /// The type's offers, replacing the old ones; at most 32. Empty keeps
    /// the type in patrons' inboxes only.
    #[schema(max_items = 32)]
    offers: Vec<OfferJson>,
}

/// Lists which notifiers patrons may pick for each patron type.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    get,
    path = "/api/notifications/offers",
    responses(
        (status = 200, description = "The offers, by type.", body = OfferTable),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn get_offers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<OfferTable>, ApiError> {
    administrator(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let offers = library
        .patron_offers()
        .await
        .map_err(|err| store_error("get_offers", err))?;
    Ok(Json(OfferTable {
        offers: offers
            .into_iter()
            .map(|(t, os)| (t, os.into_iter().map(offer_json).collect()))
            .collect(),
    }))
}

/// The wire form of an offer.
fn offer_json(o: PatronOffer) -> OfferJson {
    OfferJson {
        plugin_instance_id: o.plugin_instance_id,
        default_on: o.default_on,
    }
}

/// Replaces which notifiers patrons may pick for one patron type.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    put,
    path = "/api/notifications/offers/{type}",
    params(("type" = String, Path, description = "Notification type id.")),
    request_body = OffersBody,
    responses(
        (status = 204, description = "The offers are replaced."),
        (status = 400, description = "An unknown or operational type (either may still be cleared with no offers, to remove offers left from before the type changed), an instance that is not a personal notifier, a repeat, or more than 32.", body = ErrorMessage, example = json!({"error": "plugin instance 3f9c2b7e does not send to patrons"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The offers could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn put_offers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(notification_type): AxumPath<String>,
    Json(body): Json<OffersBody>,
) -> Result<StatusCode, ApiError> {
    administrator(&state, &headers).await?;
    let (_permit, library) = write_snapshot(&state, "put_offers").await?;
    let catalog = catalog(&state, &library).await?;
    replaceable_type(
        &catalog,
        &notification_type,
        NotificationAudience::Patron,
        body.offers.is_empty(),
    )?;
    for offer in &body.offers {
        if !catalog
            .notifier(&offer.plugin_instance_id)
            .is_some_and(|n| n.personal)
        {
            return Err(bad(format!(
                "plugin instance {} does not send to patrons",
                offer.plugin_instance_id
            )));
        }
    }
    let offers: Vec<PatronOffer> = body
        .offers
        .into_iter()
        .map(|o| PatronOffer {
            plugin_instance_id: o.plugin_instance_id,
            default_on: o.default_on,
        })
        .collect();
    library
        .set_patron_offers(&notification_type, &offers)
        .await
        .map_err(|err| store_error("put_offers", err))?;
    tracing::info!(notification_type = %notification_type, offers = offers.len(), "patron notification offers set");
    Ok(StatusCode::NO_CONTENT)
}

// --- Delivery log -----------------------------------------------------------

/// Query string of `GET /api/notifications/deliveries`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DeliveryQuery {
    /// Only this destination's deliveries.
    destination_id: Option<String>,
    /// `next_cursor` of the previous page.
    cursor: Option<String>,
    /// Page size (default 50): a whole number, where 0 is read as 1 and
    /// anything from 201 up to 18446744073709551615 as 200; a negative or
    /// non-numeric value, or one past 18446744073709551615, is refused with
    /// a plain-text 400. A page may hold fewer rows than asked when the
    /// library's database returns fewer per read.
    limit: Option<u64>,
}

/// One delivery-log row.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DeliveryJson {
    /// Delivery id.
    #[schema(example = "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b")]
    id: String,
    /// The notification.
    #[schema(example = "1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e")]
    notification_id: String,
    /// Its type.
    #[schema(example = "test")]
    notification_type: String,
    /// Its title.
    #[schema(example = "Test notification")]
    title: String,
    /// The destination.
    #[schema(example = "8a7b6c5d-4e3f-4a2b-9c1d-0e9f8a7b6c5d")]
    destination_id: String,
    /// The notifier instance.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// `pending`, `delivered`, `failed` or `dropped`.
    #[schema(example = "delivered")]
    state: String,
    /// Send attempts made.
    #[schema(example = 1)]
    attempts: i64,
    /// The service's id for the delivered message.
    #[schema(example = "1700000000.000200")]
    provider_id: Option<String>,
    /// The last failure or retry reason.
    #[schema(example = "rate_limited: slow down")]
    reason: Option<String>,
    /// When it was made (RFC 3339).
    #[schema(example = "2026-11-21T10:00:00+00:00")]
    created_at: String,
    /// When it last changed (RFC 3339).
    #[schema(example = "2026-11-21T10:00:02+00:00")]
    updated_at: String,
}

impl From<DeliveryRecord> for DeliveryJson {
    fn from(d: DeliveryRecord) -> Self {
        Self {
            id: d.id,
            notification_id: d.notification_id,
            notification_type: d.notification_type,
            title: d.title,
            destination_id: d.destination_id,
            plugin_instance_id: d.plugin_instance_id,
            state: d.state.as_str().to_string(),
            attempts: d.attempts,
            provider_id: d.provider_id,
            reason: d.reason,
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }
}

/// One page of the delivery log.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DeliveryPage {
    /// Newest first.
    deliveries: Vec<DeliveryJson>,
    /// Pass as `cursor` for the next page; `null` on the last page, where
    /// paging stops.
    #[schema(example = "eyJjcmVhdGVkX2F0IjoiMjAyNi0xMS0yMVQxMDowMDowMCswMDowMCIsImlkIjoiOWU4ZCJ9")]
    next_cursor: Option<String>,
}

/// Pages the delivery log, newest first.
///
/// Access: the operator, an Owner or an Administrator.
#[utoipa::path(
    get,
    path = "/api/notifications/deliveries",
    params(DeliveryQuery),
    responses(
        (status = 200, description = "One page of deliveries.", body = DeliveryPage),
        (status = 400, description = "The cursor is not a valid cursor; pass back this route's own `next_cursor` (JSON). Or `limit` is not a whole number that fits in 64 bits (plain text, from the query parser).", content(
            (ErrorMessage = "application/json", example = json!({"error": "invalid page cursor"})),
            (String = "text/plain", example = json!("Failed to deserialize query string: limit: invalid digit found in string")),
        )),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can manage notifications"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn list_deliveries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DeliveryQuery>,
) -> Result<Json<DeliveryPage>, ApiError> {
    administrator(&state, &headers).await?;
    let before = parse_cursor(query.cursor.as_deref())?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE)
        .clamp(1, bookclerk_library::MAX_NOTIFY_PAGE);
    let library = state.library_snapshot().await;
    let page = library
        .notify_deliveries(query.destination_id.as_deref(), before.as_ref(), limit)
        .await
        .map_err(|err| store_error("list_deliveries", err))?;
    let rows = page.items;
    let next_cursor = page.more.then(|| rows.last()).flatten().map(|last| {
        NotifyCursor {
            created_at: last.created_at.clone(),
            id: last.id.clone(),
        }
        .encode()
    });
    Ok(Json(DeliveryPage {
        deliveries: rows.into_iter().map(DeliveryJson::from).collect(),
        next_cursor,
    }))
}

// --- A patron's own settings ------------------------------------------------

/// One notifier a patron may pick for a type.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PatronOfferJson {
    /// The notifier instance.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// Its name.
    #[schema(example = "Telegram")]
    name: String,
    /// Whether the type goes there when the patron chose nothing.
    #[schema(example = true)]
    default_on: bool,
}

/// One patron type and what the patron chose for it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PatronTypeJson {
    /// Type id.
    #[schema(example = "discover:new_book")]
    id: String,
    /// Title.
    #[schema(example = "A new book you might like")]
    title: String,
    /// Notifiers offered for it.
    offers: Vec<PatronOfferJson>,
    /// Whether the patron made a choice; without one the defaults apply.
    #[schema(example = false)]
    chosen: bool,
    /// The patron's preference, not a delivery result: `false` keeps the
    /// type in the inbox only. `true` when nothing is chosen, in which case
    /// the offers' defaults pick where it goes.
    #[schema(example = true)]
    enabled: bool,
    /// The destinations the patron saved for it, when chosen. These are
    /// saved choices, not live targets: routing skips one whose notifier is
    /// no longer offered for the type, so check `offers` before showing one
    /// as eligible or sending the list back.
    #[schema(example = json!([]))]
    destination_ids: Vec<String>,
}

/// A patron's notification settings.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PatronSettings {
    /// Patron types administrators offer a notifier for.
    types: Vec<PatronTypeJson>,
    /// The patron's own destinations.
    destinations: Vec<DestinationJson>,
    /// Notifiers the patron can add a destination on.
    notifiers: Vec<NotifierJson>,
}

/// Reads the caller's notification settings: the types they can receive
/// beyond the inbox, what they chose, and their destinations.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    get,
    path = "/api/me/notifications",
    security(("portalSession" = []), ("operatorSession" = [])),
    responses(
        (status = 200, description = "The settings.", body = PatronSettings),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn get_my_settings(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
) -> Result<Json<PatronSettings>, ApiError> {
    let user = patron(&caller)?;
    let library = state.library_snapshot().await;
    let catalog = catalog(&state, &library).await?;
    let offers = library
        .patron_offers()
        .await
        .map_err(|err| store_error("get_my_settings", err))?;
    let choices: BTreeMap<String, PatronChoice> = library
        .patron_choices(user)
        .await
        .map_err(|err| store_error("get_my_settings", err))?
        .into_iter()
        .map(|c| (c.notification_type.clone(), c))
        .collect();
    let destinations = library
        .personal_destinations(user)
        .await
        .map_err(|err| store_error("get_my_settings", err))?;
    // Offers for a type no plugin declares any more, or one that is now
    // operational, stay stored until an administrator clears them; they
    // make no notifier eligible.
    let offered: BTreeSet<String> = offers
        .iter()
        .filter(|(type_id, _)| catalog.patron_type(type_id).is_some())
        .flat_map(|(_, type_offers)| type_offers)
        .map(|o| o.plugin_instance_id.clone())
        .collect();
    let mut types = Vec::new();
    for (type_id, type_offers) in offers {
        let Some(known) = catalog.patron_type(&type_id) else {
            continue;
        };
        let choice = choices.get(&type_id);
        types.push(PatronTypeJson {
            id: type_id.clone(),
            title: known.title.clone(),
            offers: type_offers
                .into_iter()
                .map(|o| PatronOfferJson {
                    name: catalog
                        .notifier(&o.plugin_instance_id)
                        .map_or_else(|| o.plugin_instance_id.clone(), |n| n.name.clone()),
                    plugin_instance_id: o.plugin_instance_id,
                    default_on: o.default_on,
                })
                .collect(),
            chosen: choice.is_some(),
            enabled: choice.is_none_or(|c| c.enabled),
            destination_ids: choice
                .map(|c| c.destination_ids.clone())
                .unwrap_or_default(),
        });
    }
    Ok(Json(PatronSettings {
        types,
        destinations: destinations
            .into_iter()
            .map(DestinationJson::from)
            .collect(),
        // Only notifiers offered for some type can take a new address; the
        // patron's existing addresses stay listed above either way, so
        // they can still be removed.
        notifiers: catalog
            .notifiers
            .into_iter()
            .filter(|n| n.personal && offered.contains(&n.plugin_instance_id))
            .collect(),
    }))
}

/// Body of `PUT /api/me/notifications/choices/{type}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChoiceBody {
    /// `false` keeps the type in the inbox only.
    #[schema(example = true)]
    enabled: bool,
    /// The caller's own destinations it goes to; at most 32. Left out or
    /// empty, the type goes to no destination outside the inbox, even
    /// with `enabled: true`; delete the choice to return to the defaults.
    #[serde(default)]
    #[schema(max_items = 32, example = json!(["8a7b6c5d-4e3f-4a2b-9c1d-0e9f8a7b6c5d"]))]
    destination_ids: Vec<String>,
}

/// Sets what the caller gets for one patron type, replacing the defaults.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    put,
    path = "/api/me/notifications/choices/{type}",
    security(("portalSession" = []), ("operatorSession" = [])),
    params(("type" = String, Path, description = "Notification type id.")),
    request_body = ChoiceBody,
    responses(
        (status = 204, description = "The choice is saved."),
        (status = 400, description = "A bad type id, a repeat, more than 32 destinations, or (when enabled) a destination on a notifier not offered for the type.", body = ErrorMessage, example = json!({"error": "destination 8a7b6c5d is chosen twice"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 404, description = "A destination is not one of the caller's.", body = ErrorMessage, example = json!({"error": "notification destination 8a7b6c5d not found"})),
        (status = 500, description = "The choice could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn put_my_choice(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    AxumPath(notification_type): AxumPath<String>,
    Json(body): Json<ChoiceBody>,
) -> Result<StatusCode, ApiError> {
    let user = patron(&caller)?;
    if !is_qualified_notification_type(&notification_type) {
        return Err(bad(format!(
            "`{notification_type}` is not a notification type id"
        )));
    }
    let (_permit, library) = write_snapshot(&state, "put_my_choice").await?;
    library
        .set_patron_choice(
            user,
            &PatronChoice {
                notification_type,
                enabled: body.enabled,
                destination_ids: body.destination_ids,
            },
        )
        .await
        .map_err(|err| store_error("put_my_choice", err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Drops the caller's choice for one type, so the defaults apply again.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    delete,
    path = "/api/me/notifications/choices/{type}",
    security(("portalSession" = []), ("operatorSession" = [])),
    params(("type" = String, Path, description = "Notification type id.")),
    responses(
        (status = 204, description = "The defaults apply again."),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 500, description = "The choice could not be removed.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn delete_my_choice(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    AxumPath(notification_type): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let user = patron(&caller)?;
    let (_permit, library) = write_snapshot(&state, "delete_my_choice").await?;
    library
        .clear_patron_choice(user, &notification_type)
        .await
        .map_err(|err| store_error("delete_my_choice", err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `POST /api/me/notifications/destinations`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewPersonalBody {
    /// A notifier instance that sends to patrons.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// The address, as the notifier's `address_fields` describe it: a flat
    /// object of strings.
    /// A key written twice is refused (`422`) rather than one value kept.
    #[schema(value_type = std::collections::BTreeMap<String, String>, example = json!({"chat_id": "123456789"}))]
    address: bookclerk_plugin_abi::notifications::AddressFields,
    /// Name shown in pickers; the notifier's own label for the address,
    /// or the notifier's name, when absent.
    #[schema(example = "My phone")]
    label: Option<String>,
}

/// The address document a patron sent, checked against the notifier's
/// declared fields before the notifier sees it.
///
/// [`parse_address_document`] is the parse the `notifier_reply` fuzz
/// target drives.
///
/// # Errors
///
/// The [`bookclerk_plugin_abi::notifications::NotificationError`] text
/// when the document is not a flat object of short strings, or misses or
/// adds a field.
pub(crate) fn personal_address(
    address: &std::collections::BTreeMap<String, String>,
    rules: &[AddressFieldRule],
) -> Result<String, String> {
    let text = serde_json::to_string(address).map_err(|err| err.to_string())?;
    let parsed = parse_address_document(&text).map_err(|err| err.to_string())?;
    check_address_against_fields(&parsed, rules).map_err(|err| err.to_string())?;
    Ok(parsed.to_json())
}

/// Adds a personal destination for the caller.
///
/// The notifier normalises and checks the address first
/// (`Notifier.validateDestination`); the host checks its answer again
/// against the declared fields before saving it.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    post,
    path = "/api/me/notifications/destinations",
    security(("portalSession" = []), ("operatorSession" = [])),
    request_body = NewPersonalBody,
    responses(
        (status = 201, description = "The destination.", body = DestinationJson),
        (status = 400, description = "A bad address or label, an instance that does not send to patrons or is not offered for any type, or a patron who already keeps 32 addresses.", body = ErrorMessage, example = json!({"error": "address field `chat_id` is required"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 409, description = "The notifier is not running on this server.", body = ErrorMessage, example = json!({"error": "the notifier is not running on this server"})),
        (status = 502, description = "The notifier could not be reached, or the host could not use its reply (for example a normalised address missing a required field).", body = ErrorMessage, example = json!({"error": "the notifier could not check the address"})),
        (status = 500, description = "The destination could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_my_destination(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<NewPersonalBody>,
) -> Result<(StatusCode, Json<DestinationJson>), ApiError> {
    let user = patron(&caller)?;
    let (_permit, library) = write_snapshot(&state, "post_my_destination").await?;
    let catalog = catalog(&state, &library).await?;
    let notifier_info = catalog
        .notifier(&body.plugin_instance_id)
        .filter(|n| n.personal)
        .ok_or_else(|| {
            bad(format!(
                "plugin instance {} does not send to patrons",
                body.plugin_instance_id
            ))
        })?;
    // An address is only useful on a notifier offered for some type:
    // routing sends patrons nowhere else.
    let offered = library
        .patron_offers()
        .await
        .map_err(|err| store_error("post_my_destination", err))?
        .iter()
        .filter(|(type_id, _)| catalog.patron_type(type_id).is_some())
        .flat_map(|(_, type_offers)| type_offers)
        .any(|o| o.plugin_instance_id == body.plugin_instance_id);
    if !offered {
        return Err(bad(format!(
            "plugin instance {} is not offered to patrons",
            body.plugin_instance_id
        )));
    }
    let rules: Vec<AddressFieldRule> = notifier_info
        .address_fields
        .iter()
        .map(|f| AddressFieldRule {
            key: f.key.clone(),
            required: f.required,
        })
        .collect();
    let address_json = personal_address(&body.address.0, &rules).map_err(bad)?;
    let notifier = {
        let integrations = state.integrations.read().await;
        integrations
            .all()
            .iter()
            .find(|i| i.plugin_instance_id() == Some(body.plugin_instance_id.as_str()))
            .cloned()
    }
    .filter(|n| n.supports_notifier())
    .ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            Json(ErrorMessage::new(
                "the notifier is not running on this server",
            )),
        )
    })?;
    // Only a host-chosen category is logged: a notifier's own message may
    // repeat the address it was sent, which can hold a secret.
    let unreachable = |category: &str| {
        tracing::warn!(notifier = %body.plugin_instance_id, category, "validateDestination failed");
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorMessage::new(
                "the notifier could not check the address",
            )),
        )
    };
    // The whole wait, including time queued behind a send on the
    // notifier's session, stays inside the route's own 8-second limit, so
    // a busy notifier answers 502 rather than a bare timeout.
    let validated = match tokio::time::timeout(
        VALIDATE_DEADLINE,
        notifier.notify_validate_destination(NotifyValidateParams { address_json }),
    )
    .await
    .unwrap_or_else(|_| {
        Err(bookclerk_integrations::IntegrationError::message(
            "the notifier did not answer in time",
        ))
    }) {
        Ok(Ok(validated)) => validated,
        Ok(Err(refused))
            if refused.code == bookclerk_plugin_abi::PluginErrorCode::InvalidParams =>
        {
            return Err(bad(format!(
                "the notifier refused the address: {}",
                bookclerk_plugin_abi::notifications::sanitize_reason(&refused.message)
            )));
        }
        Ok(Err(other)) => {
            return Err(unreachable(&format!("the notifier answered {}", other.code)).into())
        }
        Err(_) => return Err(unreachable("the notifier could not be reached").into()),
    };
    let (normalised, label) = accept_validated(validated, &rules, body.label, &notifier_info.name)
        .map_err(|()| unreachable("the notifier's answer was refused"))?;
    let created = library
        .create_notify_destination(&NewDestination {
            plugin_instance_id: body.plugin_instance_id,
            kind: DestinationKind::Personal,
            user_id: Some(user),
            label,
            address_json: normalised.to_json(),
        })
        .await
        .map_err(|err| store_error("post_my_destination", err))?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// Takes a notifier's `validateDestination` answer: the address it
/// normalised, checked again against the declared fields, and the label to
/// save (the patron's, else the notifier's suggestion, else `fallback`).
///
/// An unusable suggested label is dropped before the answer is checked, so
/// it gives way to the next label rather than refusing a good address.
///
/// # Errors
///
/// `Err(())` when the address breaks the address rules or the fields.
fn accept_validated(
    mut validated: NotifyValidated,
    rules: &[AddressFieldRule],
    patron_label: Option<String>,
    fallback: &str,
) -> Result<(NotifyAddress, String), ()> {
    let suggested = validated.label.take().filter(|l| notifier_label_fits(l));
    let address = check_validated(&validated)
        .and_then(|address| check_address_against_fields(&address, rules).map(|()| address))
        .map_err(|_| ())?;
    let label = patron_label
        .filter(|l| !l.trim().is_empty())
        .or(suggested)
        .unwrap_or_else(|| fallback_label(fallback));
    Ok((address, label))
}

/// The notifier's name made into a label the store takes: control and
/// layout characters dropped, cut at a character boundary to the label
/// cap, and "Notifier" when nothing is left. A manifest name has no length
/// limit of its own.
fn fallback_label(name: &str) -> String {
    let mut label = String::new();
    for c in name
        .chars()
        .filter(|c| !c.is_control() && !bookclerk_plugin_abi::notifications::is_layout_char(*c))
    {
        if label.len() + c.len_utf8()
            > bookclerk_library::notifications::MAX_DESTINATION_LABEL_BYTES
        {
            break;
        }
        label.push(c);
    }
    if label.trim().is_empty() {
        "Notifier".to_string()
    } else {
        label
    }
}

/// Whether a label a notifier proposed is usable as is: one non-blank line
/// within the label cap. One that is not gives way to the notifier's name,
/// so a patron who sent no label is never refused over the notifier's.
fn notifier_label_fits(label: &str) -> bool {
    !label.trim().is_empty()
        && label.len() <= bookclerk_library::notifications::MAX_DESTINATION_LABEL_BYTES
        && !label
            .chars()
            .any(|c| c.is_control() || bookclerk_plugin_abi::notifications::is_layout_char(c))
}

/// Deletes one of the caller's destinations.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    delete,
    path = "/api/me/notifications/destinations/{id}",
    security(("portalSession" = []), ("operatorSession" = [])),
    params(("id" = String, Path, description = "Destination id.")),
    responses(
        (status = 204, description = "The destination is gone."),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 404, description = "Not one of the caller's destinations.", body = ErrorMessage, example = json!({"error": "destination 8a7b6c5d not found"})),
        (status = 500, description = "The destination could not be deleted.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn delete_my_destination(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let user = patron(&caller)?;
    let (_permit, library) = write_snapshot(&state, "delete_my_destination").await?;
    let mine = library
        .notify_destination(&id)
        .await
        .map_err(|err| store_error("delete_my_destination", err))?
        .is_some_and(|d| d.kind == DestinationKind::Personal && d.user_id == Some(user));
    if !mine {
        return Err(missing(format!("destination {id} not found")));
    }
    library
        .delete_notify_destination(&id)
        .await
        .map_err(|err| store_error("delete_my_destination", err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resumes one of the caller's destinations that repeated failures paused,
/// and clears its failure count.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    post,
    path = "/api/me/notifications/destinations/{id}/resume",
    security(("portalSession" = []), ("operatorSession" = [])),
    params(("id" = String, Path, description = "Destination id.")),
    responses(
        (status = 200, description = "The destination.", body = DestinationJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 404, description = "Not one of the caller's destinations.", body = ErrorMessage, example = json!({"error": "destination 8a7b6c5d not found"})),
        (status = 500, description = "The destination could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_my_destination_resume(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DestinationJson>, ApiError> {
    let user = patron(&caller)?;
    let (_permit, library) = write_snapshot(&state, "post_my_destination_resume").await?;
    let mine = |found: Option<Destination>| {
        found
            .filter(|d| d.kind == DestinationKind::Personal && d.user_id == Some(user))
            .ok_or_else(|| missing(format!("destination {id} not found")))
    };
    mine(
        library
            .notify_destination(&id)
            .await
            .map_err(|err| store_error("post_my_destination_resume", err))?,
    )?;
    library
        .resume_notify_destination(&id)
        .await
        .map_err(|err| store_error("post_my_destination_resume", err))?;
    let resumed = mine(
        library
            .notify_destination(&id)
            .await
            .map_err(|err| store_error("post_my_destination_resume", err))?,
    )?;
    Ok(Json(resumed.into()))
}

// --- The inbox --------------------------------------------------------------

/// Query string of `GET /api/me/notifications/inbox`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct InboxQuery {
    /// `next_cursor` of the previous page.
    cursor: Option<String>,
    /// Page size (default 50): a whole number, where 0 is read as 1 and
    /// anything from 201 up to 18446744073709551615 as 200; a negative or
    /// non-numeric value, or one past 18446744073709551615, is refused with
    /// a plain-text 400. A page may hold fewer rows than asked when the
    /// library's database returns fewer per read.
    limit: Option<u64>,
}

/// One labelled value.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct FieldJson {
    /// Label.
    #[schema(example = "Narrator")]
    name: String,
    /// Value.
    #[schema(example = "Kate Reading")]
    value: String,
}

/// One inbox entry.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct InboxEntryJson {
    /// Notification id.
    #[schema(example = "1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e")]
    id: String,
    /// Type id.
    #[schema(example = "discover:new_book")]
    notification_type: String,
    /// `info`, `warning` or `critical`.
    #[schema(example = "info")]
    severity: String,
    /// Title.
    #[schema(example = "A new book you might like")]
    title: String,
    /// Plain-text body.
    #[schema(example = "The Way of Kings is on Libro.fm.")]
    text: String,
    /// Path inside the library it links to.
    #[schema(example = "/works/6c1f2e8a")]
    link_path: Option<String>,
    /// Labelled values.
    fields: Vec<FieldJson>,
    /// When it arrived (RFC 3339).
    #[schema(example = "2026-11-21T10:00:00+00:00")]
    created_at: String,
    /// When the patron read it (RFC 3339); `null` while unread.
    #[schema(example = "2026-11-21T10:05:00+00:00")]
    read_at: Option<String>,
}

impl From<InboxEntry> for InboxEntryJson {
    fn from(e: InboxEntry) -> Self {
        let n = e.notification.notification;
        Self {
            id: e.notification.id,
            notification_type: n.notification_type,
            severity: n.severity.wire_name().to_string(),
            title: n.title,
            text: n.text,
            link_path: n.link_path,
            fields: n
                .fields
                .into_iter()
                .map(|NotificationField { name, value }| FieldJson { name, value })
                .collect(),
            created_at: e.notification.created_at,
            read_at: e.read_at,
        }
    }
}

/// One page of the inbox.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct InboxPage {
    /// Newest first.
    entries: Vec<InboxEntryJson>,
    /// Unread entries in the whole inbox.
    #[schema(example = 3)]
    unread: u64,
    /// Pass as `cursor` for the next page; `null` on the last page, where
    /// paging stops.
    #[schema(example = "eyJjcmVhdGVkX2F0IjoiMjAyNi0xMS0yMVQxMDowMDowMCswMDowMCIsImlkIjoiMWIyYyJ9")]
    next_cursor: Option<String>,
}

/// Pages the caller's inbox, newest first.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    get,
    path = "/api/me/notifications/inbox",
    security(("portalSession" = []), ("operatorSession" = [])),
    params(InboxQuery),
    responses(
        (status = 200, description = "One page of the inbox.", body = InboxPage),
        (status = 400, description = "The cursor is not a valid cursor; pass back this route's own `next_cursor` (JSON). Or `limit` is not a whole number that fits in 64 bits (plain text, from the query parser).", content(
            (ErrorMessage = "application/json", example = json!({"error": "invalid page cursor"})),
            (String = "text/plain", example = json!("Failed to deserialize query string: limit: invalid digit found in string")),
        )),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn get_my_inbox(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Query(query): Query<InboxQuery>,
) -> Result<Json<InboxPage>, ApiError> {
    let user = patron(&caller)?;
    let before = parse_cursor(query.cursor.as_deref())?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE)
        .clamp(1, bookclerk_library::MAX_NOTIFY_PAGE);
    let library = state.library_snapshot().await;
    let page = library
        .inbox(user, before.as_ref(), limit)
        .await
        .map_err(|err| store_error("get_my_inbox", err))?;
    let rows = page.items;
    let unread = library
        .inbox_unread(user)
        .await
        .map_err(|err| store_error("get_my_inbox", err))?;
    let next_cursor = page.more.then(|| rows.last()).flatten().map(|last| {
        NotifyCursor {
            created_at: last.notification.created_at.clone(),
            id: last.notification.id.clone(),
        }
        .encode()
    });
    Ok(Json(InboxPage {
        entries: rows.into_iter().map(InboxEntryJson::from).collect(),
        unread,
        next_cursor,
    }))
}

/// Body of `POST /api/me/notifications/inbox/read`.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadBody {
    /// One notification to mark read; absent or `null` marks the whole inbox
    /// read.
    #[schema(example = "1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e")]
    notification_id: Option<String>,
}

/// How many entries a read marked.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReadMarked {
    /// Entries that were unread and are now read.
    #[schema(example = 1)]
    marked: u64,
}

/// Marks one inbox entry, or all of them, read.
///
/// Access: any signed-in library user, for their own settings, or an
/// operator session acting as a library user; a plain operator session or
/// the operator token has none.
#[utoipa::path(
    post,
    path = "/api/me/notifications/inbox/read",
    security(("portalSession" = []), ("operatorSession" = [])),
    request_body = ReadBody,
    responses(
        (status = 200, description = "What was marked.", body = ReadMarked),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The session has no library user.", body = ErrorMessage, example = json!({"error": "this session has no library user to keep notification settings for"})),
        (status = 404, description = "The notification is not in the caller's inbox.", body = ErrorMessage, example = json!({"error": "notification 1b2c3d4e not found"})),
        (status = 500, description = "The inbox could not be written.", body = ErrorMessage, example = json!({"error": "the library could not be read or changed"})),
    ),
)]
pub(crate) async fn post_my_inbox_read(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<ReadBody>,
) -> Result<Json<ReadMarked>, ApiError> {
    let user = patron(&caller)?;
    let (_permit, library) = write_snapshot(&state, "post_my_inbox_read").await?;
    let marked = library
        .mark_inbox_read(user, body.notification_id.as_deref())
        .await
        .map_err(|err| store_error("post_my_inbox_read", err))?;
    Ok(Json(ReadMarked { marked }))
}

#[cfg(test)]
#[path = "notifications_api_tests.rs"]
mod tests;
