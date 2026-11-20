//! [`Integration`] adapter over an external plugin process.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_integrations::{
    Brand, EventSubscription, ExternalUser, Integration, IntegrationContext, IntegrationHealth,
    ProvidedOidcClient, QueueSubscription,
};
use bookclerk_plugin_sdk::{
    AuthenticateUserParams, DomainEvent, EventResult, MetadataChapters, MetadataChaptersParams,
    MetadataLookupParams, MetadataRecord, MetadataSearchParams, ScanLibraryParams,
    PRODUCT_API_VERSION,
};

use crate::discover::DiscoveredPlugin;
use crate::rpc_session::{PluginSession, SessionServices, HOST_SHARED_ACCOUNT};
use crate::Result;

/// External integration backed by a discovered plugin binary.
pub struct ExternalIntegration {
    /// Cap'n Proto session (never given `library.db`); opened once with the
    /// granted plugin config table as the `CONFIG` binding.
    session: Arc<PluginSession>,
    /// `CONFIG` payload passed to `PluginWorker.open`.
    opened_config: bookclerk_plugin_sdk::ExtensibleConfig,
    /// `SECRETS` payload passed to `PluginWorker.open`.
    opened_secrets: bookclerk_plugin_sdk::ExtensibleConfig,
    /// Operator-facing name from describe metadata (falls back to the manifest id).
    display_name: String,
    /// Whether this integration is enabled in host config after describe.
    enabled: bool,
    /// Portal brand colors/icon leaked from describe metadata, if the guest supplied one.
    brand: Option<Brand>,
    /// When true, the host may call the guest credential-login RPC (username/password).
    allow_credential_login: bool,
    /// Durable outbox subscriptions from `plugin.toml`.
    event_subscriptions: Vec<EventSubscription>,
    /// `[[queues.consumers]]` rows from `plugin.toml`.
    queue_subscriptions: Vec<QueueSubscription>,
    /// `[triggers] crons` schedules from `plugin.toml`.
    cron_schedules: Vec<bookclerk_plugin_manifest::CronSchedule>,
    /// Generation of the host-side new-user poll from [`Self::start`]; each
    /// start and stop ends the loop before it.
    user_poll: crate::user_poll::PollGeneration,
    /// Deployment instance id. `None` until the reconciler binds one.
    plugin_instance_id: Option<String>,
}

impl ExternalIntegration {
    /// Keeps the metadata records whose kinds are inside this provider's
    /// declared media; the rest are logged and dropped.
    fn declared_records(&self, records: Vec<MetadataRecord>) -> Vec<MetadataRecord> {
        let media = &self.session.describe_snapshot().media;
        records
            .into_iter()
            .filter(|r| {
                super::declared_media::declared(
                    self.session.id(),
                    media,
                    "metadata record",
                    (r.work_kind, r.expression_kind, r.media_kind),
                )
            })
            .collect()
    }

    /// Spawn with bindings already resolved for one plugin instance.
    ///
    /// Does not look up an instance by plugin key.
    ///
    /// # Errors
    ///
    /// Returns an error when the guest cannot start or `open` fails.
    pub async fn spawn_prepared(
        plugin: &DiscoveredPlugin,
        config: &Config,
        services: SessionServices,
        prepared: crate::instance_bindings::PreparedOpen,
        allow_credential_login: bool,
    ) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(crate::PluginError::message(format!(
                "plugin `{}` api_version {} is not supported",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        let opened_config = prepared.bindings.config.clone();
        let opened_secrets = prepared.bindings.secrets.clone();
        let session = Arc::new(
            PluginSession::spawn_with(
                plugin,
                config,
                prepared.spawn_config_table.clone(),
                HOST_SHARED_ACCOUNT,
                &[],
                services,
            )
            .await?,
        );
        let describe = session.describe_snapshot();
        let display_name = describe
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_abi(describe.brand.as_ref());
        let event_subscriptions = plugin
            .manifest
            .events
            .consumers
            .iter()
            .map(|s| EventSubscription {
                event_type: s.event_type.clone(),
                schema_versions: s.schema_versions.clone(),
                supports_suspend: s.supports_suspend,
                resource_class: if s.resource_class.trim().is_empty() {
                    "network".into()
                } else {
                    s.resource_class.clone()
                },
                filter: s.filter.clone().filter(|v| !v.is_null()),
                message_retention_secs: s.message_retention_period,
            })
            .collect();
        let queue_subscriptions = plugin
            .manifest
            .queues
            .consumers
            .iter()
            .map(|c| QueueSubscription {
                queue: c.queue.clone(),
                max_retries: c.max_retries,
                dead_letter_queue: c.dead_letter_queue.clone(),
                retry_delay_secs: c.retry_delay,
                message_retention_secs: c.message_retention_period,
            })
            .collect();
        // Only schedules the operator granted tick; the session refuses the
        // rest anyway, so claiming them would only queue failing fires.
        let cron_schedules = plugin
            .manifest
            .triggers
            .crons
            .iter()
            .filter(|cron| session.grant().allows_cron(cron.as_str()))
            .cloned()
            .collect();
        session.open(prepared.bindings).await?;
        Ok(Self {
            session,
            opened_config,
            opened_secrets,
            display_name,
            enabled: true,
            brand,
            allow_credential_login,
            event_subscriptions,
            queue_subscriptions,
            cron_schedules,
            user_poll: crate::user_poll::PollGeneration::default(),
            plugin_instance_id: None,
        })
    }

    /// Records the plugin instance id used to recognize this guest on reload.
    pub fn bind_plugin_instance(&mut self, plugin_instance_id: &str) {
        self.plugin_instance_id = Some(plugin_instance_id.to_string());
    }

    /// Session opened for this integration.
    #[must_use]
    pub(crate) fn session(&self) -> &Arc<PluginSession> {
        &self.session
    }

    /// `CONFIG` JSON passed to `open`.
    #[must_use]
    pub(crate) fn opened_config(&self) -> &bookclerk_plugin_sdk::ExtensibleConfig {
        &self.opened_config
    }

    /// `SECRETS` JSON passed to `open`.
    #[must_use]
    pub(crate) fn opened_secrets(&self) -> &bookclerk_plugin_sdk::ExtensibleConfig {
        &self.opened_secrets
    }

    /// Integration health RPC.
    ///
    /// Calls `health` on each exported entrypoint that has one
    /// (`remoteLibrary`, `metadata`). Guests that export neither are ready
    /// once `open` has succeeded: event, job, OIDC, and CLI-only integrations
    /// use that describe/open check.
    ///
    /// # Errors
    ///
    /// Returns an error when an entrypoint's health reports not ok.
    pub async fn check_health(&self) -> Result<()> {
        if self
            .session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
        {
            let ok = self
                .session
                .remote_library(|src| async move { src.health().await })
                .await?;
            health_result(ok)?;
        }
        if self.session.has_entrypoint(crate::Entrypoint::Metadata) {
            let ok = self
                .session
                .metadata(|provider| async move { provider.health().await })
                .await?;
            health_result(ok)?;
        }
        if self.session.has_entrypoint(crate::Entrypoint::Covers) {
            let ok = self
                .session
                .covers(|fetcher| async move { fetcher.health().await })
                .await?;
            health_result(ok)?;
        }
        if self
            .session
            .has_entrypoint(crate::Entrypoint::MetadataFiles)
        {
            let ok = self
                .session
                .metadata_files(|files| async move { files.health().await })
                .await?;
            health_result(ok)?;
        }
        if self.session.has_entrypoint(crate::Entrypoint::Notifier) {
            let ok = self
                .session
                .notifier(|notifier| async move { notifier.health().await })
                .await?;
            health_result(ok)?;
        }
        Ok(())
    }

    /// Runs one typed `remoteLibrary` method through the plugin session.
    ///
    /// # Errors
    ///
    /// Returns when the entrypoint is missing or the guest method fails.
    async fn int_call<T, F, Fut>(&self, call: F) -> bookclerk_integrations::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn bookclerk_plugin_sdk::RemoteLibrary>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<T, bookclerk_plugin_sdk::PluginError>>
            + 'static,
    {
        Ok(self.session.remote_library(call).await?)
    }
}

/// Maps a guest `HealthOk` onto the integration health result.
///
/// # Errors
///
/// Returns the guest's detail (or `health failed`) when `ok` is false.
fn health_result(ok: bookclerk_plugin_sdk::HealthOk) -> Result<()> {
    if ok.ok {
        Ok(())
    } else {
        Err(crate::PluginError::message(if ok.detail.is_empty() {
            "health failed".to_string()
        } else {
            ok.detail
        }))
    }
}

/// Folds the health reports of every probed entrypoint into one status.
///
/// No reports (an event, job, OIDC, or CLI-only integration) is healthy.
///
/// # Returns
///
/// `ok` when every report is ok, and the non-empty details joined with
/// `; ` (`None` when there are none).
fn combine_health(reports: Vec<bookclerk_plugin_sdk::HealthOk>) -> (bool, Option<String>) {
    let ok = reports.iter().all(|report| report.ok);
    let details: Vec<String> = reports
        .into_iter()
        .map(|report| report.detail)
        .filter(|detail| !detail.is_empty())
        .collect();
    (ok, (!details.is_empty()).then(|| details.join("; ")))
}

/// Asks a guest's `remoteLibrary` for the users it has queued since the
/// last poll.
///
/// A user with an empty `provider` is returned as is; the poll loop fills
/// in the plugin alias.
///
/// # Errors
///
/// Returns the guest's `poll_events` failure, or a missing entrypoint.
async fn poll_session_users(
    session: &PluginSession,
) -> bookclerk_integrations::Result<Vec<ExternalUser>> {
    let users = session
        .remote_library(|stub| async move { stub.poll_events().await })
        .await?;
    Ok(users
        .into_iter()
        .map(|user| ExternalUser {
            provider: user.provider,
            external_user_id: user.external_user_id,
            display_name: user.display_name,
            access_token: None,
        })
        .collect())
}

impl ExternalIntegration {
    /// Asks this integration's resident poll loop, when one runs, to poll
    /// the guest's new users now.
    pub(crate) fn poll_users_now(&self) {
        self.user_poll.poll_now();
    }

    /// Asks the guest for the users it has queued since the last poll.
    ///
    /// # Returns
    ///
    /// No users when the guest exports no `remoteLibrary`.
    ///
    /// # Errors
    ///
    /// Returns the guest's `poll_events` failure.
    pub(crate) async fn poll_external_users(
        &self,
    ) -> bookclerk_integrations::Result<Vec<ExternalUser>> {
        if !self
            .session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
        {
            return Ok(Vec::new());
        }
        poll_session_users(&self.session).await
    }

    /// The guest's relying-party templates from its `oidcClients` RPC.
    ///
    /// Unlike [`Integration::provided_oidc_clients`], a failed RPC is an
    /// error here rather than an empty list, so a caller that caches the
    /// answer can tell a failure from a guest that offers no clients.
    ///
    /// # Returns
    ///
    /// No clients when the guest exports no `oidc` entrypoint: client
    /// registration must match credential login.
    ///
    /// # Errors
    ///
    /// Returns the guest's `oidcClients` failure.
    pub(crate) async fn fetch_oidc_clients(
        &self,
    ) -> bookclerk_integrations::Result<Vec<ProvidedOidcClient>> {
        if !self.session.has_entrypoint(crate::Entrypoint::Oidc) {
            return Ok(Vec::new());
        }
        let clients = self.session.oidc_clients().await?;
        Ok(clients
            .into_iter()
            .map(|t| {
                let display_name = t.display_name_or_id().to_string();
                let default_scopes = t.scopes_or_default();
                ProvidedOidcClient {
                    client_id: t.client_id,
                    display_name,
                    callback_path: t.callback_path,
                    public_client: t.public_client,
                    default_scopes,
                    issue_refresh_token: t.issue_refresh_token,
                    origin_config_key: t.origin_config_key,
                }
            })
            .collect())
    }
}

#[async_trait]
impl Integration for ExternalIntegration {
    fn id(&self) -> &str {
        self.session.alias()
    }

    fn plugin_key(&self) -> &str {
        self.session.id()
    }

    fn plugin_instance_id(&self) -> Option<&str> {
        self.plugin_instance_id.as_deref()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn start(&self, ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
        let remote_library = self
            .session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary);
        if remote_library {
            let _ = self
                .int_call(|stub| async move { stub.start().await })
                .await;
        }
        // The host polls `poll_events` and hands each user to the daemon's
        // hook (claim tickets); the plugin does not know what the host does
        // with them. Under an on-demand proxy the proxy owns this poll and
        // passes no hook here, so a woken guest starts no loop of its own.
        if remote_library {
            if let Some(on_user) = ctx.on_external_user {
                let session = Arc::clone(&self.session);
                crate::user_poll::spawn(
                    &self.user_poll,
                    crate::user_poll::RESIDENT_USER_POLL_INTERVAL,
                    self.id().to_string(),
                    on_user,
                    move || {
                        let session = Arc::clone(&session);
                        async move { Some(poll_session_users(&session).await) }
                    },
                );
            }
        }
        Ok(())
    }

    async fn stop(&self) -> bookclerk_integrations::Result<()> {
        self.user_poll.end();
        if self
            .session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
        {
            let _ = self.int_call(|stub| async move { stub.stop().await }).await;
        }
        Ok(())
    }

    async fn deliver_domain_event(
        &self,
        event: DomainEvent,
    ) -> bookclerk_integrations::Result<EventResult> {
        self.deliver_domain_event_cancelable(event, Arc::new(AtomicBool::new(false)))
            .await
    }

    async fn deliver_domain_event_cancelable(
        &self,
        event: DomainEvent,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<EventResult> {
        if !self.session.consumes_events() {
            return Ok(EventResult::Retry {
                retry_at_unix_ms: 0,
                reason: "plugin declares no [[events.consumers]]".into(),
            });
        }
        let mut results = self.session.deliver_events(vec![event], cancel).await?;
        results.pop().ok_or_else(|| {
            bookclerk_integrations::IntegrationError::message(
                "plugin returned no result for the delivered event",
            )
        })
    }

    fn event_subscriptions(&self) -> Vec<EventSubscription> {
        self.event_subscriptions.clone()
    }

    fn queue_subscriptions(&self) -> Vec<QueueSubscription> {
        self.queue_subscriptions.clone()
    }

    fn cron_schedules(&self) -> Vec<bookclerk_plugin_manifest::CronSchedule> {
        self.cron_schedules.clone()
    }

    async fn run_scheduled_cancelable(
        &self,
        invocation: bookclerk_integrations::ScheduledInvocation,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<()> {
        if !self.cron_schedules.contains(&invocation.cron) {
            return Err(bookclerk_integrations::IntegrationError::message(format!(
                "plugin declares no cron `{}`",
                invocation.cron
            )));
        }
        let scheduled_time = u64::try_from(invocation.scheduled_time_ms).map_err(|_| {
            bookclerk_integrations::IntegrationError::message(
                "cron fire time is before the Unix epoch",
            )
        })?;
        self.session
            .run_scheduled(
                bookclerk_plugin_abi::ScheduledController {
                    cron: invocation.cron.as_str().to_string(),
                    scheduled_time,
                    fire_id: invocation.job_id,
                    attempt: invocation.attempt,
                    deadline_unix_ms: invocation.deadline_unix_ms,
                },
                cancel,
            )
            .await?;
        Ok(())
    }

    async fn deliver_queue_batch_cancelable(
        &self,
        batch: bookclerk_integrations::QueueBatch,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<Vec<bookclerk_integrations::QueueResult>> {
        if !self.session.consumes_queues() {
            return Err(bookclerk_integrations::IntegrationError::message(
                "plugin declares no [[queues.consumers]]",
            ));
        }
        Ok(self.session.deliver_queue(batch, cancel).await?)
    }

    async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
        let mut reports = Vec::new();
        if self
            .session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
        {
            reports.push(
                self.int_call(|stub| async move { stub.health().await })
                    .await?,
            );
        }
        if self.session.has_entrypoint(crate::Entrypoint::Metadata) {
            reports.push(
                self.session
                    .metadata(|provider| async move { provider.health().await })
                    .await?,
            );
        }
        if self.session.has_entrypoint(crate::Entrypoint::Covers) {
            reports.push(
                self.session
                    .covers(|fetcher| async move { fetcher.health().await })
                    .await?,
            );
        }
        if self
            .session
            .has_entrypoint(crate::Entrypoint::MetadataFiles)
        {
            reports.push(
                self.session
                    .metadata_files(|files| async move { files.health().await })
                    .await?,
            );
        }
        if self.session.has_entrypoint(crate::Entrypoint::Notifier) {
            reports.push(
                self.session
                    .notifier(|notifier| async move { notifier.health().await })
                    .await?,
            );
        }
        let (ok, detail) = combine_health(reports);
        Ok(IntegrationHealth {
            id: self.id().to_string(),
            enabled: self.enabled,
            ok,
            detail,
        })
    }

    fn supports_library_scan(&self) -> bool {
        self.session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
    }

    async fn scan_library(&self, force: bool) -> bookclerk_integrations::Result<()> {
        self.int_call(
            move |stub| async move { stub.scan_library(ScanLibraryParams { force }).await },
        )
        .await
    }

    fn supports_listening_sync(&self) -> bool {
        self.session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
    }

    async fn sync_listening_progress(
        &self,
        library: &bookclerk_library::LibraryStore,
    ) -> bookclerk_integrations::Result<usize> {
        let rows = self
            .int_call(|stub| async move { stub.sync_listening().await })
            .await?;
        let items: Vec<bookclerk_integrations::ListeningProgressSnapshot> = rows
            .into_iter()
            .map(|row| bookclerk_integrations::ListeningProgressSnapshot {
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
                last_listened_at: row
                    .last_listened_at_unix_ms
                    .and_then(|ms| i64::try_from(ms).ok())
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis),
            })
            .collect();
        bookclerk_integrations::upsert_listening_snapshots(library, self.id(), &items).await
    }

    async fn diagnose(&self) -> bookclerk_integrations::Result<Vec<String>> {
        // Only a remote library answers `diagnose`; a plugin with just
        // metadata, covers, metadata files or a notifier falls through to health.
        if self
            .session
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
        {
            let lines = self
                .int_call(|stub| async move { stub.diagnose().await })
                .await?;
            if !lines.is_empty() {
                return Ok(lines);
            }
        }
        let h = self.health().await?;
        Ok(vec![format!(
            "{} enabled={} ok={} {}",
            h.id,
            h.enabled,
            h.ok,
            h.detail.unwrap_or_default()
        )])
    }

    fn supports_metadata(&self) -> bool {
        self.session.has_entrypoint(crate::Entrypoint::Metadata)
    }

    fn supports_covers(&self) -> bool {
        self.session.has_entrypoint(crate::Entrypoint::Covers)
    }

    async fn fetch_covers(
        &self,
        params: bookclerk_plugin_sdk::CoverFetchParams,
    ) -> bookclerk_integrations::Result<Vec<bookclerk_integrations::FetchedCover>> {
        Ok(self
            .session
            .covers(move |fetcher| async move {
                let sink = super::cover_sink::CoverSink::new(params.urls.len(), params.max_bytes);
                let images = fetcher.fetch_covers(params, sink.destination()).await?;
                Ok(sink.take_fetched(images))
            })
            .await?)
    }

    async fn metadata_lookup(
        &self,
        params: MetadataLookupParams,
    ) -> bookclerk_integrations::Result<Vec<MetadataRecord>> {
        let records = self
            .session
            .metadata(move |provider| async move { provider.lookup(params).await })
            .await?;
        Ok(self.declared_records(records))
    }

    async fn metadata_search(
        &self,
        params: MetadataSearchParams,
    ) -> bookclerk_integrations::Result<Vec<MetadataRecord>> {
        let records = self
            .session
            .metadata(move |provider| async move { provider.search(params).await })
            .await?;
        Ok(self.declared_records(records))
    }

    async fn metadata_chapters(
        &self,
        params: MetadataChaptersParams,
    ) -> bookclerk_integrations::Result<MetadataChapters> {
        Ok(self
            .session
            .metadata(move |provider| async move { provider.chapters(params).await })
            .await?)
    }

    fn supports_credential_login(&self) -> bool {
        self.allow_credential_login && self.session.has_entrypoint(crate::Entrypoint::Oidc)
    }

    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
    ) -> bookclerk_integrations::Result<ExternalUser> {
        self.session.require_binding("secrets")?;
        let params = AuthenticateUserParams {
            username: username.to_string(),
            password: password.to_string(),
        };
        let user = self.session.oidc_authenticate_user(params).await?;
        Ok(ExternalUser {
            provider: if user.provider.is_empty() {
                self.id().to_string()
            } else {
                user.provider
            },
            external_user_id: user.external_user_id,
            display_name: user.display_name,
            access_token: user.access_token,
        })
    }

    fn portal_brand(&self) -> Option<Brand> {
        self.brand
    }

    async fn provided_oidc_clients(
        &self,
    ) -> bookclerk_integrations::Result<Vec<ProvidedOidcClient>> {
        match self.fetch_oidc_clients().await {
            Ok(clients) => Ok(clients),
            Err(err) => {
                tracing::debug!(
                    plugin = %self.session.id(),
                    %err,
                    "oidcClients RPC unavailable; falling back to plugin.toml"
                );
                Ok(Vec::new())
            }
        }
    }
}

/// Copies a describe brand into a `'static` [`Brand`] (strings are leaked once at load).
fn brand_from_abi(brand: Option<&bookclerk_plugin_sdk::Brand>) -> Option<Brand> {
    let b = brand.filter(|b| !b.id.is_empty())?;
    Some(Brand {
        id: Box::leak(b.id.clone().into_boxed_str()),
        name: Box::leak(b.name.clone().into_boxed_str()),
        bg: Box::leak(b.bg.clone().into_boxed_str()),
        fg: Box::leak(b.fg.clone().into_boxed_str()),
        accent: Box::leak(b.accent.clone().into_boxed_str()),
        icon_url: Box::leak(b.icon_url.clone().unwrap_or_default().into_boxed_str()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_health_requires_every_report_ok() {
        let health = |ok: bool, detail: &str| bookclerk_plugin_sdk::HealthOk {
            ok,
            detail: detail.to_string(),
        };
        assert_eq!(combine_health(Vec::new()), (true, None));
        assert_eq!(combine_health(vec![health(true, "")]), (true, None));
        assert_eq!(
            combine_health(vec![
                health(true, "library ok"),
                health(false, "catalog down")
            ]),
            (false, Some("library ok; catalog down".to_string()))
        );
    }

    #[test]
    fn health_result_maps_not_ok_to_the_guest_detail() {
        let health = |ok: bool, detail: &str| bookclerk_plugin_sdk::HealthOk {
            ok,
            detail: detail.to_string(),
        };
        assert!(health_result(health(true, "")).is_ok());
        let err = health_result(health(false, "catalog down")).expect_err("not ok");
        assert!(err.to_string().contains("catalog down"), "{err}");
        let err = health_result(health(false, "")).expect_err("not ok");
        assert!(err.to_string().contains("health failed"), "{err}");
    }
}
