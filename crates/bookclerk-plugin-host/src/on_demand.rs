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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

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

/// A running guest the proxy forwards to, with the process facts it needs.
pub(crate) trait AwakeGuest: Integration {
    /// True while the guest process is still running.
    fn guest_running(&self) -> bool;

    /// Native guest pid, when there is one.
    fn guest_pid(&self) -> Option<u32>;
}

impl AwakeGuest for crate::ExternalIntegration {
    fn guest_running(&self) -> bool {
        self.session().guest_running()
    }

    fn guest_pid(&self) -> Option<u32> {
        self.session().guest_pid()
    }
}

/// Starts the guest for one instance: spawn, `open`, health and `start`.
#[async_trait]
pub(crate) trait GuestStarter: Send + Sync {
    /// Spawns a fresh guest and returns it once it is ready for calls.
    ///
    /// # Errors
    ///
    /// Returns the spawn, `open` or health failure as text.
    async fn start_guest(
        &self,
        ctx: IntegrationContext,
    ) -> std::result::Result<Arc<dyn AwakeGuest>, String>;
}

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

/// Calls in flight and when the last one finished.
struct Activity {
    /// Calls that hold a [`Busy`] guard right now.
    in_flight: usize,
    /// When the guest last started or a call last finished.
    last_used: Instant,
}

/// Integration registry entry that starts its guest on the first call and
/// stops it after [`OnDemandIntegration::idle_timeout`] without calls.
pub struct OnDemandIntegration {
    /// Answers given while the guest sleeps.
    identity: Identity,
    /// Starts a fresh guest on wake.
    starter: Box<dyn GuestStarter>,
    /// Idle time after which the guest is stopped.
    idle_timeout: Duration,
    /// The running guest, or `None` while asleep. Held across a wake so
    /// concurrent callers share one spawn.
    guest: tokio::sync::Mutex<Option<Arc<dyn AwakeGuest>>>,
    /// In-flight calls and last use; the sweep reads it.
    activity: std::sync::Mutex<Activity>,
    /// Lifecycle context passed to each guest's `start`.
    context: std::sync::Mutex<IntegrationContext>,
    /// Set by [`Integration::stop`]; a retired proxy never wakes again.
    retired: AtomicBool,
    /// Guests started after the first, for logs and tests.
    wakes: AtomicU64,
}

/// Marks one call in flight until dropped, then records the finish time.
struct Busy<'a> {
    /// Proxy whose activity this guard holds.
    owner: &'a OnDemandIntegration,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut activity = self.owner.activity();
        activity.in_flight = activity.in_flight.saturating_sub(1);
        activity.last_used = Instant::now();
    }
}

impl OnDemandIntegration {
    /// Wraps `guest`, already started by the deployment, and begins the
    /// idle sweep.
    ///
    /// # Arguments
    ///
    /// * `guest` - the running guest; its answers become the sleep-time identity.
    /// * `plugin_instance_id` - deployment instance the guest serves.
    /// * `starter` - spawns a replacement guest on each wake.
    /// * `context` - lifecycle context the guest was started with.
    /// * `idle_timeout` - idle time before the guest is stopped.
    ///
    /// # Returns
    ///
    /// The shared proxy to register in place of the guest.
    pub(crate) fn wrap(
        guest: Arc<dyn AwakeGuest>,
        plugin_instance_id: &str,
        starter: Box<dyn GuestStarter>,
        context: IntegrationContext,
        idle_timeout: Duration,
    ) -> Arc<Self> {
        let proxy = Arc::new(Self {
            identity: Identity::of(guest.as_ref(), plugin_instance_id),
            starter,
            idle_timeout,
            guest: tokio::sync::Mutex::new(Some(guest)),
            activity: std::sync::Mutex::new(Activity {
                in_flight: 0,
                last_used: Instant::now(),
            }),
            context: std::sync::Mutex::new(context),
            retired: AtomicBool::new(false),
            wakes: AtomicU64::new(0),
        });
        spawn_idle_sweep(Arc::downgrade(&proxy), idle_timeout);
        proxy
    }

    /// Idle time after which the guest is stopped.
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// True once the deployment stopped this proxy.
    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// True while a guest is running for this proxy.
    ///
    /// Returns false while a wake or a sleep holds the guest slot.
    #[must_use]
    pub fn is_awake(&self) -> bool {
        self.guest
            .try_lock()
            .is_ok_and(|guest| guest.as_ref().is_some_and(|guest| guest.guest_running()))
    }

    /// Native pid of the running guest, if awake.
    #[must_use]
    pub fn guest_pid(&self) -> Option<u32> {
        self.guest
            .try_lock()
            .ok()
            .and_then(|guest| guest.as_ref().and_then(|guest| guest.guest_pid()))
    }

    /// Guests started on demand since the proxy was created.
    #[must_use]
    pub fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::SeqCst)
    }

    /// Locks the activity record.
    ///
    /// # Panics
    ///
    /// Panics when the activity lock is poisoned.
    fn activity(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.activity.lock().expect("on-demand activity")
    }

    /// Lifecycle context for the next guest.
    ///
    /// # Panics
    ///
    /// Panics when the context lock is poisoned.
    fn context(&self) -> IntegrationContext {
        self.context.lock().expect("on-demand context").clone()
    }

    /// Marks a call in flight, then returns the running guest, starting one
    /// when the proxy is asleep or its guest exited.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy is retired or the guest cannot start.
    async fn awake(&self) -> bookclerk_integrations::Result<(Arc<dyn AwakeGuest>, Busy<'_>)> {
        // Counted before taking the slot so a sweep that wins the slot
        // first sees the call and leaves the guest running.
        self.activity().in_flight += 1;
        let busy = Busy { owner: self };
        if self.is_retired() {
            return Err(IntegrationError::message(format!(
                "integration `{}` was stopped",
                self.identity.id
            )));
        }
        let mut slot = self.guest.lock().await;
        if let Some(guest) = slot.as_ref() {
            if guest.guest_running() {
                return Ok((Arc::clone(guest), busy));
            }
        }
        if let Some(exited) = slot.take() {
            let _ = exited.stop().await;
        }
        let started = Instant::now();
        let guest = self
            .starter
            .start_guest(self.context())
            .await
            .map_err(|detail| {
                IntegrationError::message(format!(
                    "integration `{}` did not start on demand: {detail}",
                    self.identity.id
                ))
            })?;
        self.wakes.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(
            plugin_instance_id = %self.identity.plugin_instance_id,
            elapsed_ms = started.elapsed().as_millis(),
            "integration started on demand"
        );
        *slot = Some(Arc::clone(&guest));
        Ok((guest, busy))
    }

    /// Stops the guest when no call is in flight and the last one finished
    /// at least the idle timeout ago.
    ///
    /// A wake or call holding the slot makes this a no-op; the next sweep
    /// tries again.
    ///
    /// # Returns
    ///
    /// True when a running guest was stopped.
    pub async fn sleep_if_idle(&self) -> bool {
        let Ok(mut slot) = self.guest.try_lock() else {
            return false;
        };
        if slot.is_none() {
            return false;
        }
        {
            let activity = self.activity();
            if activity.in_flight > 0 || activity.last_used.elapsed() < self.idle_timeout {
                return false;
            }
        }
        let Some(guest) = slot.take() else {
            return false;
        };
        drop(slot);
        if let Err(err) = guest.stop().await {
            tracing::warn!(
                plugin_instance_id = %self.identity.plugin_instance_id,
                error = %err,
                "idle integration stop failed"
            );
        }
        tracing::debug!(
            plugin_instance_id = %self.identity.plugin_instance_id,
            "idle integration stopped"
        );
        true
    }
}

/// Checks `proxy` for idleness a few times per timeout until it is dropped
/// or retired.
fn spawn_idle_sweep(proxy: Weak<OnDemandIntegration>, idle_timeout: Duration) {
    let period = (idle_timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(15));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(proxy) = proxy.upgrade() else {
                return;
            };
            if proxy.is_retired() {
                return;
            }
            proxy.sleep_if_idle().await;
        }
    });
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

    async fn start(&self, ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
        *self.context.lock().expect("on-demand context") = ctx.clone();
        let guest = self.guest.lock().await.clone();
        match guest {
            Some(guest) => guest.start(ctx).await,
            None => Ok(()),
        }
    }

    async fn stop(&self) -> bookclerk_integrations::Result<()> {
        self.retired.store(true, Ordering::SeqCst);
        let guest = self.guest.lock().await.take();
        match guest {
            Some(guest) => guest.stop().await,
            None => Ok(()),
        }
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
        let guest = self.guest.lock().await.clone();
        match guest {
            Some(guest) if guest.guest_running() => guest.health().await,
            _ => Ok(IntegrationHealth {
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
