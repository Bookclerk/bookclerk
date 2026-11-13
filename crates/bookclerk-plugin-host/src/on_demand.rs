//! On-demand activation for integrations whose work arrives as triggers.
//!
//! An [`OnDemandIntegration`] stands in the integration registry for one
//! deployed plugin instance whether or not its guest is running. The event,
//! queue and cron workers find it the way they find a resident integration;
//! the first call that needs the guest starts it (spawn, jail, `describe`,
//! `open`), and a sweep stops it again once it has had no call in flight for
//! the idle timeout. Subscriptions, schedules and leases are owned by the
//! deployment (see `docs/adr/event-delivery-model.md`), so nothing about the
//! plugin's catalog changes while it sleeps.
//!
//! Only plugins that [`activates_on_demand`] accepts are wrapped: their
//! entrypoints are triggers or stateless lookups, and their runtime starts
//! in tens of milliseconds (see the `cold_start` measurement in
//! `bookclerk-plugin-e2e`). Storefronts, storage, remote libraries, sign-in
//! providers and Python guests stay resident for now.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_integrations::{
    Brand, EventSubscription, ExternalUser, FetchedCover, Integration, IntegrationContext,
    IntegrationError, IntegrationHealth, ProvidedOidcClient, QueueSubscription,
    ScheduledInvocation,
};
use bookclerk_plugin_abi::{
    DomainEvent, EventResult, MetadataChapters, MetadataChaptersParams, MetadataLookupParams,
    MetadataRecord, MetadataSearchParams, QueueBatch, QueueResult,
};
use bookclerk_plugin_manifest::CronSchedule;

use crate::activation::{Activation, Busy, Guest, GuestStarter, OnDemandStatus, WakeError};
use crate::{Entrypoint, PluginFamily, PluginManifest};

/// How long a trigger-driven guest stays up after its last call returns.
///
/// One minute matches the event delivery lease, so a burst of deliveries
/// reuses one process while a quiet plugin is gone within a minute.
pub const TRIGGER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// True when a deployment of `manifest` may sleep while idle and start on
/// the next call.
///
/// The plugin must belong only to the integration family and export no
/// entrypoint that keeps session state or answers sign-in
/// (`remoteLibrary`, `oidc`). A `cli` entrypoint is fine: the CLI starts its
/// own session for each command. Its runtime must not be Python, whose
/// guests take seconds to start (15 s and more on a quarter CPU).
///
/// # Arguments
///
/// * `manifest` - the installed manifest the deployment spawns.
#[must_use]
pub fn activates_on_demand(manifest: &PluginManifest) -> bool {
    manifest.families() == [PluginFamily::Integration]
        && !manifest.declares_python()
        && !manifest.has_entrypoint(Entrypoint::RemoteLibrary)
        && !manifest.has_entrypoint(Entrypoint::Oidc)
}

/// A running integration guest the proxy forwards to.
pub(crate) trait AwakeGuest: Integration + Guest {}

impl<T: Integration + Guest> AwakeGuest for T {}

#[async_trait]
impl Guest for crate::ExternalIntegration {
    fn guest_running(&self) -> bool {
        self.session().guest_running()
    }

    fn guest_pid(&self) -> Option<u32> {
        self.session().guest_pid()
    }

    async fn stop_guest(&self) -> std::result::Result<(), String> {
        Integration::stop(self).await.map_err(|err| err.to_string())
    }
}

/// Lifecycle context shared between a proxy and its starter, so a guest
/// started on demand gets the context the daemon last gave `start`.
pub(crate) type SharedContext = Arc<std::sync::Mutex<IntegrationContext>>;

/// The values a sleeping proxy answers without waking its guest.
///
/// Captured from the guest the deployment first started, so they match what
/// a resident integration would report.
#[derive(Clone)]
struct Identity {
    /// Display alias (`Integration::id`).
    id: String,
    /// Provenance-qualified plugin key.
    plugin_key: String,
    /// Deployment instance id.
    plugin_instance_id: String,
    /// Operator-facing name from `describe`.
    display_name: String,
    /// `[[events.consumers]]` rows.
    event_subscriptions: Vec<EventSubscription>,
    /// `[[queues.consumers]]` rows.
    queue_subscriptions: Vec<QueueSubscription>,
    /// Granted `[triggers] crons`.
    cron_schedules: Vec<CronSchedule>,
    /// Whether the guest exports `metadata`.
    supports_metadata: bool,
    /// Whether the guest exports `covers`.
    supports_covers: bool,
    /// Whether the guest offers credential login.
    supports_credential_login: bool,
    /// Whether the guest scans a remote library.
    supports_library_scan: bool,
    /// Whether the guest syncs listening progress.
    supports_listening_sync: bool,
    /// Portal brand from `describe`.
    portal_brand: Option<Brand>,
}

impl Identity {
    /// Reads every sleep-time answer from a running guest.
    fn of(guest: &dyn AwakeGuest, plugin_instance_id: &str) -> Self {
        Self {
            id: guest.id().to_string(),
            plugin_key: guest.plugin_key().to_string(),
            plugin_instance_id: plugin_instance_id.to_string(),
            display_name: guest.display_name().to_string(),
            event_subscriptions: guest.event_subscriptions(),
            queue_subscriptions: guest.queue_subscriptions(),
            cron_schedules: guest.cron_schedules(),
            supports_metadata: guest.supports_metadata(),
            supports_covers: guest.supports_covers(),
            supports_credential_login: guest.supports_credential_login(),
            supports_library_scan: guest.supports_library_scan(),
            supports_listening_sync: guest.supports_listening_sync(),
            portal_brand: guest.portal_brand(),
        }
    }
}

/// Integration registry entry that starts its guest on the first call and
/// stops it after the idle timeout without calls.
pub struct OnDemandIntegration {
    /// Answers given while the guest sleeps.
    identity: Identity,
    /// The running guest, started on a call and stopped when idle.
    activation: Arc<Activation<dyn AwakeGuest>>,
    /// Lifecycle context passed to each guest's `start`; shared with the
    /// starter.
    context: SharedContext,
}

impl OnDemandIntegration {
    /// Wraps `guest`, already started by the deployment, and begins the
    /// idle sweep.
    ///
    /// # Arguments
    ///
    /// * `guest` - the running guest; its answers become the sleep-time identity.
    /// * `plugin_instance_id` - deployment instance the guest serves.
    /// * `starter` - spawns a replacement guest on each wake, reading `context`.
    /// * `context` - lifecycle context, shared with `starter`.
    /// * `idle_timeout` - idle time before the guest is stopped.
    ///
    /// # Returns
    ///
    /// The shared proxy to register in place of the guest.
    pub(crate) fn wrap(
        guest: Arc<dyn AwakeGuest>,
        plugin_instance_id: &str,
        starter: Box<dyn GuestStarter<dyn AwakeGuest>>,
        context: SharedContext,
        idle_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity: Identity::of(guest.as_ref(), plugin_instance_id),
            activation: Activation::start(guest, plugin_instance_id, starter, idle_timeout),
            context,
        })
    }

    /// Stops the guest now if it has been idle for the timeout.
    ///
    /// # Returns
    ///
    /// True when a running guest was stopped.
    pub async fn sleep_if_idle(&self) -> bool {
        self.activation.sleep_if_idle().await
    }

    /// Marks a call in flight and returns the running guest, starting one
    /// when asleep.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy is retired or the guest cannot start.
    async fn awake(&self) -> bookclerk_integrations::Result<(Arc<dyn AwakeGuest>, Busy<'_>)> {
        self.activation.awake().await.map_err(|err| match err {
            WakeError::Retired => {
                IntegrationError::message(format!("integration `{}` was stopped", self.identity.id))
            }
            WakeError::Start(detail) => IntegrationError::message(format!(
                "integration `{}` did not start on demand: {detail}",
                self.identity.id
            )),
        })
    }
}

#[async_trait]
impl OnDemandStatus for OnDemandIntegration {
    fn is_retired(&self) -> bool {
        self.activation.is_retired()
    }

    async fn retire(&self) {
        if let Err(err) = self.activation.retire().await {
            tracing::warn!(
                integration = %self.identity.id,
                error = %err,
                "retired on-demand guest stop failed"
            );
        }
    }

    fn is_awake(&self) -> bool {
        self.activation.is_awake()
    }

    fn guest_pid(&self) -> Option<u32> {
        self.activation.guest_pid()
    }

    fn wakes(&self) -> u64 {
        self.activation.wakes()
    }

    fn idle_timeout(&self) -> Duration {
        self.activation.idle_timeout()
    }
}

#[async_trait]
impl Integration for OnDemandIntegration {
    fn id(&self) -> &str {
        &self.identity.id
    }

    fn plugin_key(&self) -> &str {
        &self.identity.plugin_key
    }

    fn plugin_instance_id(&self) -> Option<&str> {
        Some(&self.identity.plugin_instance_id)
    }

    fn display_name(&self) -> &str {
        &self.identity.display_name
    }

    /// Stores `ctx` for later wakes and starts a running guest with it.
    ///
    /// Starting a stopped proxy revives it: a deployment whose replacement
    /// failed to start registers the previous proxy again, and its next call
    /// wakes a fresh guest.
    async fn start(&self, ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
        self.activation.revive();
        *self
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ctx.clone();
        match self.activation.running().await {
            Some(guest) => guest.start(ctx).await,
            None => Ok(()),
        }
    }

    async fn stop(&self) -> bookclerk_integrations::Result<()> {
        self.activation
            .retire()
            .await
            .map_err(IntegrationError::message)
    }

    async fn deliver_domain_event(
        &self,
        event: DomainEvent,
    ) -> bookclerk_integrations::Result<EventResult> {
        let (guest, _busy) = self.awake().await?;
        guest.deliver_domain_event(event).await
    }

    async fn deliver_domain_event_cancelable(
        &self,
        event: DomainEvent,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<EventResult> {
        let (guest, _busy) = self.awake().await?;
        guest.deliver_domain_event_cancelable(event, cancel).await
    }

    fn event_subscriptions(&self) -> Vec<EventSubscription> {
        self.identity.event_subscriptions.clone()
    }

    fn queue_subscriptions(&self) -> Vec<QueueSubscription> {
        self.identity.queue_subscriptions.clone()
    }

    fn cron_schedules(&self) -> Vec<CronSchedule> {
        self.identity.cron_schedules.clone()
    }

    async fn run_scheduled_cancelable(
        &self,
        invocation: ScheduledInvocation,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.run_scheduled_cancelable(invocation, cancel).await
    }

    async fn deliver_queue_batch_cancelable(
        &self,
        batch: QueueBatch,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<Vec<QueueResult>> {
        let (guest, _busy) = self.awake().await?;
        guest.deliver_queue_batch_cancelable(batch, cancel).await
    }

    /// Health of the running guest; a sleeping guest is reported healthy
    /// without being woken, since it passed health when it last started and
    /// the next call starts and checks it again.
    async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
        match self.activation.running().await {
            Some(guest) => guest.health().await,
            None => Ok(IntegrationHealth {
                id: self.identity.id.clone(),
                enabled: true,
                ok: true,
                detail: Some("asleep; starts on its next call".into()),
            }),
        }
    }

    fn supports_credential_login(&self) -> bool {
        self.identity.supports_credential_login
    }

    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
    ) -> bookclerk_integrations::Result<ExternalUser> {
        let (guest, _busy) = self.awake().await?;
        guest.authenticate_user(username, password).await
    }

    fn supports_library_scan(&self) -> bool {
        self.identity.supports_library_scan
    }

    async fn scan_library(&self, force: bool) -> bookclerk_integrations::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.scan_library(force).await
    }

    fn supports_listening_sync(&self) -> bool {
        self.identity.supports_listening_sync
    }

    async fn sync_listening_progress(
        &self,
        library: &bookclerk_library::LibraryStore,
    ) -> bookclerk_integrations::Result<usize> {
        let (guest, _busy) = self.awake().await?;
        guest.sync_listening_progress(library).await
    }

    fn supports_metadata(&self) -> bool {
        self.identity.supports_metadata
    }

    async fn metadata_lookup(
        &self,
        params: MetadataLookupParams,
    ) -> bookclerk_integrations::Result<Vec<MetadataRecord>> {
        let (guest, _busy) = self.awake().await?;
        guest.metadata_lookup(params).await
    }

    async fn metadata_search(
        &self,
        params: MetadataSearchParams,
    ) -> bookclerk_integrations::Result<Vec<MetadataRecord>> {
        let (guest, _busy) = self.awake().await?;
        guest.metadata_search(params).await
    }

    async fn metadata_chapters(
        &self,
        params: MetadataChaptersParams,
    ) -> bookclerk_integrations::Result<MetadataChapters> {
        let (guest, _busy) = self.awake().await?;
        guest.metadata_chapters(params).await
    }

    fn supports_covers(&self) -> bool {
        self.identity.supports_covers
    }

    async fn fetch_covers(
        &self,
        params: bookclerk_plugin_abi::CoverFetchParams,
    ) -> bookclerk_integrations::Result<Vec<FetchedCover>> {
        let (guest, _busy) = self.awake().await?;
        guest.fetch_covers(params).await
    }

    async fn diagnose(&self) -> bookclerk_integrations::Result<Vec<String>> {
        let (guest, _busy) = self.awake().await?;
        guest.diagnose().await
    }

    fn portal_brand(&self) -> Option<Brand> {
        self.identity.portal_brand
    }

    async fn provided_oidc_clients(
        &self,
    ) -> bookclerk_integrations::Result<Vec<ProvidedOidcClient>> {
        let (guest, _busy) = self.awake().await?;
        guest.provided_oidc_clients().await
    }
}

#[cfg(test)]
#[path = "on_demand_tests.rs"]
mod tests;
