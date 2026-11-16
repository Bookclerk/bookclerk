//! Guest-process helpers for the external Audiobookshelf integration plugin.
//!
//! Opaque config JSON (from `[integrations.audiobookshelf]` / spawn config) drives
//! the HTTP client. The user watch reports users created on the server from
//! [`guest_event_poll`], which the host calls on its own schedule; the host
//! owns claim-ticket and portal workflows.

use std::collections::HashSet;
use std::sync::Arc;

use bookclerk_integrations::{ExternalUser, IntegrationError, Result};
use bookclerk_plugin_sdk::sockets::{
    text_heartbeat, SocketConnectRequest, SocketFrame, SocketMessage, Sockets,
};
use bookclerk_plugin_sdk::{
    DatabaseBinding, EventPollResult, ExternalUser as AbiExternalUser, HealthOk, ListeningProgress,
};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::client::AbsApiClient;
use crate::integration::AudiobookshelfConfig;
use crate::listening::collect_listening_changes;
use crate::push;
use crate::watch::UserWatch;

/// Integration id written on listening rows and external-user records.
const PROVIDER: &str = "audiobookshelf";

/// Shared guest state for the ABS external plugin process.
pub struct AbsGuestState {
    /// Parsed `[integrations.audiobookshelf]` / spawn config JSON.
    config: AudiobookshelfConfig,
    /// HTTP client when `api_key` is present and the base URL parsed.
    client: Option<AbsApiClient>,
    /// Why the client is missing (empty API key or constructor failure).
    config_error: Option<String>,
    /// Keys of the ABS users already reported, when the plugin database is
    /// not granted (see [`crate::watch`]).
    known_users: HashSet<String>,
}

impl AbsGuestState {
    /// Build guest state from opaque spawn-config / settings JSON.
    #[must_use]
    pub fn from_config_json(config: &Value) -> Self {
        let parsed: AudiobookshelfConfig =
            serde_json::from_value(config.clone()).unwrap_or_default();
        Self::from_config(parsed)
    }

    /// Build guest state from the `open` bindings: `CONFIG` plus `SECRETS`.
    ///
    /// The host keeps secret settings out of `CONFIG`: an `api_key` set with
    /// `bookclerk plugins config set audiobookshelf api_key --secret` arrives
    /// only in `SECRETS`, so it fills `api_key` when `CONFIG` has none.
    ///
    /// # Arguments
    ///
    /// * `config` - The `CONFIG` binding's JSON (instance settings).
    /// * `secrets` - The `SECRETS` binding's JSON (unsealed secret settings).
    #[must_use]
    pub fn from_bindings_json(config: &Value, secrets: &Value) -> Self {
        let mut parsed: AudiobookshelfConfig =
            serde_json::from_value(config.clone()).unwrap_or_default();
        if parsed
            .api_key
            .as_deref()
            .is_none_or(|key| key.trim().is_empty())
        {
            parsed.api_key = ["api_key", "apiKey"]
                .iter()
                .filter_map(|key| secrets.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .find(|key| !key.is_empty())
                .map(str::to_owned);
        }
        Self::from_config(parsed)
    }

    /// Build guest state from a typed ABS config.
    #[must_use]
    pub fn from_config(config: AudiobookshelfConfig) -> Self {
        let api_key = config.api_key.clone().filter(|s| !s.is_empty());
        let (client, config_error) = match api_key {
            None => (
                None,
                Some(
                    "the audiobookshelf `api_key` secret is required (bookclerk plugins config set audiobookshelf api_key --secret)"
                        .to_string(),
                ),
            ),
            Some(api_key) => match AbsApiClient::new(config.base_url.clone(), api_key) {
                Ok(client) => (Some(client), None),
                Err(err) => (None, Some(err.to_string())),
            },
        };
        Self {
            config,
            client,
            config_error,
            known_users: HashSet::new(),
        }
    }

    /// Returns the live ABS client, or the stored config-error message.
    fn require_client(&self) -> Result<&AbsApiClient> {
        self.client.as_ref().ok_or_else(|| {
            IntegrationError::message(
                self.config_error
                    .clone()
                    .unwrap_or_else(|| "audiobookshelf integration is misconfigured".into()),
            )
        })
    }
}

/// Seeds the user watch with the users the server already has.
///
/// Runs each time the guest starts. With the plugin database, only the
/// first start ever records users, and the users a server has then are
/// never reported; without it, every start seeds its in-memory set again.
/// A failed seed is logged; the next [`guest_event_poll`] then seeds the
/// empty set instead and reports nobody.
///
/// # Arguments
///
/// * `state` - Shared guest state with the ABS client and config.
/// * `memory` - The plugin's `LISTENING_STATE` database, when granted.
///
/// # Errors
///
/// Returns an error when the integration has no usable client.
pub async fn guest_start(
    state: Arc<Mutex<AbsGuestState>>,
    memory: Option<&DatabaseBinding>,
) -> Result<()> {
    let mut g = state.lock().await;
    let client = g.require_client()?.clone();
    if !g.config.watch_users {
        debug!("ABS guest user watch disabled");
        return Ok(());
    }
    if memory.is_none() {
        warn!(
            "ABS user watch has no LISTENING_STATE database; users created while the guest sleeps are not reported"
        );
    }
    match client.list_users().await {
        Ok(users) => {
            let mut watch = UserWatch::new(memory, &mut g.known_users);
            match watch.seed(&users).await {
                Ok(0) => {}
                Ok(count) => info!(count, "seeded ABS guest user watch set"),
                Err(err) => warn!(%err, "failed to seed the ABS user watch"),
            }
        }
        Err(err) => warn!(%err, "failed to seed ABS guest users"),
    }
    Ok(())
}

/// Reports the ABS users created since the last poll.
///
/// Lists the server's users and returns those the watch has not reported
/// (see [`crate::watch`]). When the watch was never seeded (the seed at
/// start failed), this poll seeds it and reports nobody.
///
/// # Arguments
///
/// * `state` - Shared guest state with the ABS client and config.
/// * `memory` - The plugin's `LISTENING_STATE` database, when granted.
///
/// # Errors
///
/// Returns an error when the client is missing, ABS cannot list its users,
/// or the watch database fails.
pub async fn guest_event_poll(
    state: &Mutex<AbsGuestState>,
    memory: Option<&DatabaseBinding>,
) -> Result<EventPollResult> {
    let mut g = state.lock().await;
    if !g.config.watch_users {
        return Ok(EventPollResult::default());
    }
    let client = g.require_client()?.clone();
    let users = client.list_users().await?;
    let mut watch = UserWatch::new(memory, &mut g.known_users);
    let watch_err = |err: bookclerk_plugin_sdk::PluginError| {
        IntegrationError::message(format!("ABS user watch: {err}"))
    };
    if watch.seed(&users).await.map_err(watch_err)? > 0 {
        return Ok(EventPollResult::default());
    }
    let fresh = watch.take_new(users).await.map_err(watch_err)?;
    Ok(EventPollResult {
        users: fresh
            .into_iter()
            .map(|user| {
                info!(user_id = %user.id, username = %user.username, "ABS guest user observed");
                AbiExternalUser {
                    provider: PROVIDER.into(),
                    external_user_id: user.id,
                    display_name: Some(user.username),
                    access_token: None,
                }
            })
            .collect(),
    })
}

/// Probes ABS connectivity and returns the typed health reply for the host status strip.
///
/// Misconfiguration (no client) yields `ok: false` with a detail message rather
/// than an RPC error so diagnose/health UIs can render it.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_health(state: &Mutex<AbsGuestState>) -> Result<HealthOk> {
    let g = state.lock().await;
    let Some(client) = g.client.as_ref() else {
        return Ok(HealthOk {
            ok: false,
            detail: g
                .config_error
                .clone()
                .unwrap_or_else(|| "audiobookshelf integration is misconfigured".into()),
        });
    };
    match client.authorize().await {
        Ok(auth) => Ok(HealthOk {
            ok: true,
            detail: auth
                .user
                .map(|u| format!("authorized as {}", u.username))
                .unwrap_or_default(),
        }),
        Err(err) => Ok(HealthOk {
            ok: false,
            detail: err.to_string(),
        }),
    }
}

/// Runs connectivity checks and returns human-readable diagnose lines (libraries listed).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_diagnose(state: &Mutex<AbsGuestState>) -> Result<Vec<String>> {
    let g = state.lock().await;
    let client = g.require_client()?;
    let auth = client.authorize().await?;
    let mut lines = Vec::new();
    if let Some(user) = auth.user {
        lines.push(format!("authorized as {} ({})", user.username, user.id));
    } else {
        lines.push("authorized (no user in response)".into());
    }
    for lib in client.list_libraries().await? {
        lines.push(format!("library {} — {}", lib.id, lib.name));
    }
    Ok(lines)
}

/// Asks Audiobookshelf to rescan the configured library (post-acquire hook).
///
/// # Arguments
///
/// * `state` - Shared guest state with ABS client + config.
/// * `force` - When true, request a forced rescan from ABS.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_scan_library(state: &Mutex<AbsGuestState>, force: bool) -> Result<()> {
    let g = state.lock().await;
    let client = g.require_client()?;
    let Some(library_id) = g.config.library_id.as_deref().filter(|s| !s.is_empty()) else {
        return Err(IntegrationError::message(
            "integrations.audiobookshelf.library_id unset",
        ));
    };
    client.scan_library(library_id, force).await
}

/// Collect listening progress as typed ABI rows (host upserts).
///
/// With the `LISTENING_STATE` database only rows that changed since the
/// last sync come back, and each user's whole snapshot once a day (see
/// [`collect_listening_changes`]); without it every row does.
///
/// # Errors
///
/// Returns an error when ABS cannot list its users.
pub async fn guest_sync_listening(
    state: &Mutex<AbsGuestState>,
    memory: Option<&DatabaseBinding>,
) -> Result<Vec<ListeningProgress>> {
    let g = state.lock().await;
    let client = g.require_client()?;
    let snapshots = collect_listening_changes(client, memory, chrono::Utc::now()).await?;
    Ok(snapshots
        .into_iter()
        .map(|row| ListeningProgress {
            external_user_id: row.external_user_id,
            external_item_id: row.external_item_id,
            identity_id: row.identity_id,
            title: row.title,
            authors: row.authors,
            asin: row.asin,
            isbn: row.isbn,
            progress: row.progress,
            current_time_seconds: row.current_time_seconds,
            duration_seconds: row.duration_seconds,
            is_finished: row.is_finished,
            last_listened_at_unix_ms: row
                .last_listened_at
                .and_then(|ts| u64::try_from(ts.timestamp_millis()).ok()),
        })
        .collect())
}

/// Username/password login against ABS (when allowed).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_authenticate_user(
    state: &Mutex<AbsGuestState>,
    username: &str,
    password: &str,
) -> Result<ExternalUser> {
    let g = state.lock().await;
    if !g.config.allow_credential_login {
        return Err(IntegrationError::message(
            "integrations.audiobookshelf.allow_credential_login is false",
        ));
    }
    g.require_client()?
        .authenticate_user(username, password)
        .await
}

/// Handle host-forwarded integration events (e.g. book_acquired → scan).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_on_event(state: &Mutex<AbsGuestState>, params: &Value) -> Result<()> {
    let event = params
        .get("type")
        .or_else(|| params.get("event"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if event != "book_acquired" {
        return Ok(());
    }
    let (notify, library_id, client) = {
        let g = state.lock().await;
        let notify = g.config.notify_scan_on_acquire;
        let library_id = g.config.library_id.clone();
        let client = g.require_client()?.clone();
        (notify, library_id, client)
    };
    if !notify {
        return Ok(());
    }
    let Some(library_id) = library_id.filter(|s| !s.is_empty()) else {
        warn!("ABS guest scan skipped: integrations.audiobookshelf.library_id unset");
        return Ok(());
    };
    info!(%library_id, "ABS guest triggering library scan after book_acquired");
    client.scan_library(&library_id, false).await
}

/// Opens the pushed-event socket when none is open, so the host learns of a
/// new ABS user as soon as the server announces it.
///
/// Does nothing without the `SOCKETS` binding, with the user watch off, or
/// without an API key. A socket that closes is not reopened at once: the
/// next start or poll opens it again, so a server that keeps refusing the
/// key is retried at the poll's pace rather than in a loop. The host answers
/// the server's Engine.IO ping itself, so an open socket costs no wakes.
///
/// # Arguments
///
/// * `state` - Shared guest state with the ABS config.
/// * `sockets` - The plugin's `SOCKETS` binding, when granted.
///
/// # Returns
///
/// `true` when a socket was opened by this call.
///
/// # Errors
///
/// Returns the host's error when the open sockets cannot be listed or the
/// socket cannot be opened.
pub async fn guest_ensure_push(
    state: &Mutex<AbsGuestState>,
    sockets: Option<&dyn Sockets>,
) -> Result<bool> {
    let Some(sockets) = sockets else {
        return Ok(false);
    };
    let url = {
        let g = state.lock().await;
        if !g.config.watch_users || g.client.is_none() {
            return Ok(false);
        }
        let Some(url) = push::socket_url(&g.config.base_url) else {
            return Ok(false);
        };
        url
    };
    let socket_err = |err: bookclerk_plugin_sdk::PluginError| {
        IntegrationError::message(format!("ABS push: {err}"))
    };
    if !sockets.list().await.map_err(socket_err)?.is_empty() {
        return Ok(false);
    }
    sockets
        .connect(SocketConnectRequest {
            url,
            auto_responses: vec![text_heartbeat(push::PING, push::PONG)],
            ..SocketConnectRequest::default()
        })
        .await
        .map_err(socket_err)?;
    info!("opened the ABS pushed-event socket");
    Ok(true)
}

/// Answers one frame pushed on the ABS socket.
///
/// Sends the handshake replies (joining the namespace, then authenticating
/// with the API key), closes the socket when the server rejects the key,
/// and reports whether the frame announced a new user.
///
/// # Arguments
///
/// * `state` - Shared guest state with the API key.
/// * `sockets` - The plugin's `SOCKETS` binding.
/// * `event` - The pushed frame.
///
/// # Returns
///
/// `true` when the server announced a user created on it.
///
/// # Errors
///
/// Returns the host's error when a reply cannot be sent.
pub async fn guest_push_frame(
    state: &Mutex<AbsGuestState>,
    sockets: &dyn Sockets,
    event: &SocketMessage,
) -> Result<bool> {
    let SocketFrame::Text(text) = &event.frame else {
        return Ok(false);
    };
    let api_key = {
        let g = state.lock().await;
        g.config.api_key.clone().unwrap_or_default()
    };
    let reaction = push::on_frame(text, &api_key);
    let socket_err = |err: bookclerk_plugin_sdk::PluginError| {
        IntegrationError::message(format!("ABS push: {err}"))
    };
    if let Some(reply) = reaction.reply {
        sockets
            .send(event.socket.clone(), SocketFrame::Text(reply))
            .await
            .map_err(socket_err)?;
    }
    if reaction.close {
        warn!("ABS closed the pushed-event channel; the next poll opens it again");
        sockets
            .close(event.socket.clone(), 1000, String::new())
            .await
            .map_err(socket_err)?;
    }
    Ok(reaction.new_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Whether the state built a client, i.e. found an API key.
    fn has_client(state: &AbsGuestState) -> bool {
        state.client.is_some()
    }

    #[test]
    fn api_key_comes_from_secrets_when_config_has_none() {
        let config = json!({ "base_url": "http://127.0.0.1:13378" });
        let state = AbsGuestState::from_bindings_json(&config, &json!({ "api_key": "k" }));
        assert!(has_client(&state));
        assert!(state.config_error.is_none());
    }

    #[test]
    fn api_key_in_config_wins_over_secrets() {
        let config = json!({ "base_url": "http://127.0.0.1:13378", "api_key": "from-config" });
        let state =
            AbsGuestState::from_bindings_json(&config, &json!({ "api_key": "from-secrets" }));
        assert_eq!(state.config.api_key.as_deref(), Some("from-config"));
    }

    #[test]
    fn blank_or_missing_secret_leaves_the_key_missing() {
        let config = json!({ "base_url": "http://127.0.0.1:13378" });
        for secrets in [json!({}), json!({ "api_key": "  " }), json!(null)] {
            let state = AbsGuestState::from_bindings_json(&config, &secrets);
            assert!(!has_client(&state));
            assert!(state.config_error.is_some());
        }
    }

    /// ABS server listing users with the given ids.
    async fn server(ids: &[&str]) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let users: Vec<Value> = ids
            .iter()
            .map(|id| json!({ "id": id, "username": format!("{id}-name") }))
            .collect();
        wiremock::Mock::given(method("GET"))
            .and(path("/api/users"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({ "users": users })),
            )
            .mount(&server)
            .await;
        server
    }

    /// Guest state for `server`, as one guest process would build it.
    fn state_for(server: &wiremock::MockServer, watch_users: bool) -> Arc<Mutex<AbsGuestState>> {
        let config = json!({ "base_url": server.uri(), "watch_users": watch_users });
        Arc::new(Mutex::new(AbsGuestState::from_bindings_json(
            &config,
            &json!({ "api_key": "k" }),
        )))
    }

    /// A fresh `LISTENING_STATE` database with every migration applied,
    /// under `profile`.
    async fn database(profile: bookclerk_db_testkit::Profile) -> DatabaseBinding {
        let unit = bookclerk_db_testkit::BindingDatabase::open(
            profile,
            crate::listening_migrations_for(crate::LISTENING_STATE_BINDING),
        )
        .await
        .unwrap();
        DatabaseBinding::from_session(Arc::new(unit))
    }

    /// The external user ids a poll reported.
    fn reported(result: &EventPollResult) -> Vec<&str> {
        result
            .users
            .iter()
            .map(|user| user.external_user_id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn a_user_created_while_the_guest_slept_is_reported_by_the_next_guest() {
        for profile in bookclerk_db_testkit::Profile::ALL {
            let db = database(profile).await;
            let before = server(&["root", "ada"]).await;
            let first = state_for(&before, true);
            guest_start(Arc::clone(&first), Some(&db)).await.unwrap();
            let quiet = guest_event_poll(&first, Some(&db)).await.unwrap();
            assert!(quiet.users.is_empty(), "seeded users are never reported");

            // The guest slept; a new process starts against the grown server.
            let after = server(&["root", "ada", "grace"]).await;
            let woken = state_for(&after, true);
            guest_start(Arc::clone(&woken), Some(&db)).await.unwrap();
            let polled = guest_event_poll(&woken, Some(&db)).await.unwrap();
            assert_eq!(reported(&polled), ["grace"], "{profile:?}");
            assert_eq!(polled.users[0].provider, PROVIDER);
            assert_eq!(polled.users[0].display_name.as_deref(), Some("grace-name"));
            assert!(guest_event_poll(&woken, Some(&db))
                .await
                .unwrap()
                .users
                .is_empty());
        }
    }

    #[tokio::test]
    async fn a_poll_seeds_the_watch_when_the_start_could_not() {
        let abs = server(&["root"]).await;
        let state = state_for(&abs, true);
        let first = guest_event_poll(&state, None).await.unwrap();
        assert!(first.users.is_empty());
        let grown = server(&["root", "ada"]).await;
        let config = json!({ "base_url": grown.uri(), "watch_users": true });
        let mut g = state.lock().await;
        let known = std::mem::take(&mut g.known_users);
        *g = AbsGuestState::from_bindings_json(&config, &json!({ "api_key": "k" }));
        g.known_users = known;
        drop(g);
        let next = guest_event_poll(&state, None).await.unwrap();
        assert_eq!(reported(&next), ["ada"]);
    }

    #[tokio::test]
    async fn a_disabled_watch_reports_nobody() {
        let abs = server(&["root", "ada"]).await;
        let state = state_for(&abs, false);
        guest_start(Arc::clone(&state), None).await.unwrap();
        assert!(guest_event_poll(&state, None)
            .await
            .unwrap()
            .users
            .is_empty());
        assert!(abs.received_requests().await.unwrap_or_default().is_empty());
    }

    /// What a fake `SOCKETS` binding was asked to do.
    #[derive(Debug, PartialEq)]
    enum SocketCall {
        Connect(String, usize),
        Send(String, String),
        Close(String, u16),
    }

    /// A `SOCKETS` binding that records calls and holds `open` sockets.
    #[derive(Default)]
    struct FakeSockets {
        open: std::cell::RefCell<Vec<String>>,
        calls: std::cell::RefCell<Vec<SocketCall>>,
    }

    #[async_trait::async_trait(?Send)]
    impl Sockets for FakeSockets {
        async fn connect(
            &self,
            request: SocketConnectRequest,
        ) -> std::result::Result<
            bookclerk_plugin_sdk::sockets::SocketInfo,
            bookclerk_plugin_sdk::PluginError,
        > {
            assert_eq!(
                request.auto_responses,
                vec![text_heartbeat("2", "3")],
                "the host answers the server's ping"
            );
            self.calls.borrow_mut().push(SocketCall::Connect(
                request.url.clone(),
                request.auto_responses.len(),
            ));
            self.open.borrow_mut().push("s1".into());
            Ok(bookclerk_plugin_sdk::sockets::SocketInfo {
                socket: "s1".into(),
                url: request.url,
                ..Default::default()
            })
        }

        async fn send(
            &self,
            socket: String,
            frame: SocketFrame,
        ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
            let SocketFrame::Text(text) = frame else {
                panic!("binary frame sent");
            };
            self.calls.borrow_mut().push(SocketCall::Send(socket, text));
            Ok(())
        }

        async fn close(
            &self,
            socket: String,
            code: u16,
            _reason: String,
        ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
            self.open.borrow_mut().retain(|open| *open != socket);
            self.calls
                .borrow_mut()
                .push(SocketCall::Close(socket, code));
            Ok(())
        }

        async fn set_attachment(
            &self,
            _socket: String,
            _attachment: Vec<u8>,
        ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
            Ok(())
        }

        async fn list(
            &self,
        ) -> std::result::Result<
            Vec<bookclerk_plugin_sdk::sockets::SocketInfo>,
            bookclerk_plugin_sdk::PluginError,
        > {
            Ok(self
                .open
                .borrow()
                .iter()
                .map(|socket| bookclerk_plugin_sdk::sockets::SocketInfo {
                    socket: socket.clone(),
                    ..Default::default()
                })
                .collect())
        }
    }

    /// A text frame pushed on socket `s1`.
    fn pushed(text: &str) -> SocketMessage {
        SocketMessage {
            socket: "s1".into(),
            frame: SocketFrame::Text(text.into()),
            attachment: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_push_socket_is_opened_once() {
        let state = Arc::new(Mutex::new(AbsGuestState::from_bindings_json(
            &json!({ "base_url": "https://abs.example/" }),
            &json!({ "api_key": "k" }),
        )));
        let sockets = FakeSockets::default();
        assert!(guest_ensure_push(&state, Some(&sockets)).await.unwrap());
        assert!(!guest_ensure_push(&state, Some(&sockets)).await.unwrap());
        assert_eq!(
            *sockets.calls.borrow(),
            vec![SocketCall::Connect(
                "wss://abs.example/socket.io/?EIO=4&transport=websocket".into(),
                1
            )]
        );
        assert!(
            !guest_ensure_push(&state, None).await.unwrap(),
            "no binding"
        );
    }

    #[tokio::test]
    async fn no_push_socket_without_a_watch_or_a_key() {
        let sockets = FakeSockets::default();
        for (config, secrets) in [
            (
                json!({ "base_url": "https://abs.example", "watch_users": false }),
                json!({ "api_key": "k" }),
            ),
            (json!({ "base_url": "https://abs.example" }), json!({})),
        ] {
            let state = Mutex::new(AbsGuestState::from_bindings_json(&config, &secrets));
            assert!(!guest_ensure_push(&state, Some(&sockets)).await.unwrap());
        }
        assert!(sockets.calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn the_push_handshake_authenticates_and_reports_new_users() {
        let state = Mutex::new(AbsGuestState::from_bindings_json(
            &json!({ "base_url": "https://abs.example" }),
            &json!({ "api_key": "secret-key" }),
        ));
        let sockets = FakeSockets::default();
        sockets.open.borrow_mut().push("s1".into());
        assert!(
            !guest_push_frame(&state, &sockets, &pushed(r#"0{"sid":"a"}"#))
                .await
                .unwrap()
        );
        assert!(
            !guest_push_frame(&state, &sockets, &pushed(r#"40{"sid":"b"}"#))
                .await
                .unwrap()
        );
        assert!(
            guest_push_frame(&state, &sockets, &pushed(r#"42["user_added",{"id":"u9"}]"#))
                .await
                .unwrap()
        );
        let binary = SocketMessage {
            frame: SocketFrame::Binary(vec![1]),
            ..pushed("")
        };
        assert!(!guest_push_frame(&state, &sockets, &binary).await.unwrap());
        assert!(
            !guest_push_frame(&state, &sockets, &pushed(r#"42["invalid_token"]"#))
                .await
                .unwrap()
        );
        assert_eq!(
            *sockets.calls.borrow(),
            vec![
                SocketCall::Send("s1".into(), "40".into()),
                SocketCall::Send("s1".into(), r#"42["auth","secret-key"]"#.into()),
                SocketCall::Close("s1".into(), 1000),
            ]
        );
        assert!(sockets.open.borrow().is_empty());
    }
}
