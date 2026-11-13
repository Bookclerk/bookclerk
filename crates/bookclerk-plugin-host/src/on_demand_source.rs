//! On-demand activation for storefronts.
//!
//! An [`OnDemandSource`] stands in the source registry for one deployed
//! storefront instance whether or not its guest is running. Scans, fetches,
//! sign-ins and catalog calls start the guest; the shared
//! [`Activation`](crate::activation) core stops it after
//! [`STOREFRONT_IDLE_TIMEOUT`] without a call. Every storefront call returns
//! only after its work is done (a fetch hands back files on disk), so a
//! call in flight is all that has to keep the guest up. Credentials a guest
//! refreshes in memory are written back to the host as they change, so a
//! fresh guest starts from the stored ones.
//!
//! Account listing reads the library only and never wakes the guest.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_library::SourceScope;
use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ContentSource, ExpandSeed, FetchOptions,
    ImportCredentialsOptions, LoginOptions, OAuthProgress, PortalAuthMode, PurchaseHintOpts,
    ScanOptions, ScanSummary, SourceAccount, SourceBrand, SourceConfigOption, SourceError,
    SourceFetch, SourcePurchaseHint,
};

use crate::activation::{Activation, Busy, Guest, GuestStarter, OnDemandStatus, WakeError};
use crate::{PluginFamily, PluginManifest};

/// How long a storefront stays up after its last call returns.
///
/// Storefront work comes in bursts (a scan pages through a library, an
/// acquire fetches several parts, Discover asks for a page of hints), so
/// the timeout is longer than for trigger-driven plugins.
pub const STOREFRONT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// True when a storefront deployment of `manifest` may sleep while idle.
///
/// The plugin must belong only to the source family and must not be
/// Python, whose guests take seconds to start.
///
/// # Arguments
///
/// * `manifest` - the installed manifest the deployment spawns.
#[must_use]
pub fn storefront_activates_on_demand(manifest: &PluginManifest) -> bool {
    manifest.families() == [PluginFamily::Source] && !manifest.declares_python()
}

/// A running storefront guest the proxy forwards to.
pub(crate) trait AwakeSource: ContentSource + Guest {}

impl<T: ContentSource + Guest> AwakeSource for T {}

/// The values a sleeping proxy answers without waking its guest, captured
/// from the guest the deployment first started.
struct Identity {
    /// Storefront id (the session alias).
    id: String,
    /// Provenance-qualified plugin key.
    plugin_key: String,
    /// Deployment instance id.
    plugin_instance_id: String,
    /// Operator-facing name from `describe`.
    display_name: String,
    /// Extra storefront ids from `describe`.
    aliases: &'static [&'static str],
    /// `oauth` or password sign-in.
    portal_auth_mode: PortalAuthMode,
    /// UI brand from `describe`.
    portal_brand: SourceBrand,
    /// Registry sort order.
    sort_key: u32,
    /// Static config options the storefront advertises.
    config_options: &'static [SourceConfigOption],
    /// Whether catalog detail answers ISBN lookups.
    answers_catalog_detail_by_isbn: bool,
}

impl Identity {
    /// Reads every sleep-time answer from a running guest.
    fn of(guest: &dyn AwakeSource, plugin_instance_id: &str) -> Self {
        Self {
            id: guest.id().to_string(),
            plugin_key: guest.plugin_key().to_string(),
            plugin_instance_id: plugin_instance_id.to_string(),
            display_name: guest.display_name().to_string(),
            aliases: guest.aliases(),
            portal_auth_mode: guest.portal_auth_mode(),
            portal_brand: guest.portal_brand(),
            sort_key: guest.sort_key(),
            config_options: guest.config_options(),
            answers_catalog_detail_by_isbn: guest.answers_catalog_detail_by_isbn(),
        }
    }
}

/// Source registry entry that starts its storefront guest on the first call
/// and stops it after the idle timeout without calls.
pub struct OnDemandSource {
    /// Answers given while the guest sleeps.
    identity: Identity,
    /// The running guest, started on a call and stopped when idle.
    activation: Arc<Activation<dyn AwakeSource>>,
}

impl OnDemandSource {
    /// Wraps `guest`, already started by the deployment, and begins the
    /// idle sweep.
    ///
    /// # Arguments
    ///
    /// * `guest` - the running storefront; its answers become the sleep-time identity.
    /// * `plugin_instance_id` - deployment instance the guest serves.
    /// * `starter` - spawns a replacement guest on each wake.
    /// * `idle_timeout` - idle time before the guest is stopped.
    ///
    /// # Returns
    ///
    /// The shared proxy to register in place of the guest.
    pub(crate) fn wrap(
        guest: Arc<dyn AwakeSource>,
        plugin_instance_id: &str,
        starter: Box<dyn GuestStarter<dyn AwakeSource>>,
        idle_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity: Identity::of(guest.as_ref(), plugin_instance_id),
            activation: Activation::start(guest, plugin_instance_id, starter, idle_timeout),
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
    async fn awake(&self) -> bookclerk_source::Result<(Arc<dyn AwakeSource>, Busy<'_>)> {
        self.activation.awake().await.map_err(|err| match err {
            WakeError::Retired => {
                SourceError::api(format!("storefront `{}` was stopped", self.identity.id))
            }
            WakeError::Start(detail) => SourceError::api(format!(
                "storefront `{}` did not start on demand: {detail}",
                self.identity.id
            )),
        })
    }
}

#[async_trait]
impl OnDemandStatus for OnDemandSource {
    fn is_retired(&self) -> bool {
        self.activation.is_retired()
    }

    async fn retire(&self) {
        // A storefront has no stop RPC; dropping the session ends it, so
        // the stop cannot fail.
        let _ = self.activation.retire().await;
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
impl ContentSource for OnDemandSource {
    fn id(&self) -> &str {
        &self.identity.id
    }

    fn plugin_key(&self) -> &str {
        &self.identity.plugin_key
    }

    fn plugin_instance_id(&self) -> Option<&str> {
        Some(&self.identity.plugin_instance_id)
    }

    fn guest_pid(&self) -> Option<u32> {
        self.activation.guest_pid()
    }

    fn display_name(&self) -> &str {
        &self.identity.display_name
    }

    fn aliases(&self) -> &'static [&'static str] {
        self.identity.aliases
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        self.identity.portal_auth_mode
    }

    fn portal_brand(&self) -> SourceBrand {
        self.identity.portal_brand
    }

    fn sort_key(&self) -> u32 {
        self.identity.sort_key
    }

    async fn login(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        let (guest, _busy) = self.awake().await?;
        guest.login(scope, opts).await
    }

    async fn login_with_oauth_progress(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
        on_progress: &(dyn Fn(OAuthProgress) + Send + Sync),
    ) -> bookclerk_source::Result<SourceAccount> {
        let (guest, _busy) = self.awake().await?;
        guest
            .login_with_oauth_progress(scope, opts, on_progress)
            .await
    }

    async fn import_credentials(
        &self,
        scope: &SourceScope,
        path: &Path,
        opts: ImportCredentialsOptions,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        let (guest, _busy) = self.awake().await?;
        guest.import_credentials(scope, path, opts).await
    }

    async fn revoke_credentials(
        &self,
        scope: &SourceScope,
        account_id: &str,
    ) -> bookclerk_source::Result<()> {
        // Revoking must remove the stored credentials even when no guest can
        // start, as the resident source does after a failed sign-out.
        let (guest, _busy) = match self.awake().await {
            Ok(awake) => awake,
            Err(err) => {
                tracing::warn!(
                    account = %account_id,
                    error = %err,
                    "storefront unavailable for sign-out; deleting stored credentials anyway"
                );
                return bookclerk_source::revoke_credentials_default(scope, account_id).await;
            }
        };
        guest.revoke_credentials(scope, account_id).await
    }

    /// Reads the library's account rows without waking the guest.
    async fn list_accounts(
        &self,
        scope: &SourceScope,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        crate::host::list_scope_accounts(scope).await
    }

    async fn scan(
        &self,
        scope: &SourceScope,
        opts: ScanOptions,
    ) -> bookclerk_source::Result<ScanSummary> {
        let (guest, _busy) = self.awake().await?;
        guest.scan(scope, opts).await
    }

    async fn fetch_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> bookclerk_source::Result<SourceFetch> {
        let (guest, _busy) = self.awake().await?;
        guest.fetch_title(scope, account_id, title_id, opts).await
    }

    fn config_options(&self) -> &'static [SourceConfigOption] {
        self.identity.config_options
    }

    async fn inspect_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> bookclerk_source::Result<serde_json::Value> {
        let (guest, _busy) = self.awake().await?;
        guest.inspect_title(scope, account_id, title_id, opts).await
    }

    async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        let (guest, _busy) = self.awake().await?;
        guest.search_catalog(opts).await
    }

    async fn catalog_detail(
        &self,
        product_id: &str,
    ) -> bookclerk_source::Result<Option<CatalogHit>> {
        let (guest, _busy) = self.awake().await?;
        guest.catalog_detail(product_id).await
    }

    fn answers_catalog_detail_by_isbn(&self) -> bool {
        self.identity.answers_catalog_detail_by_isbn
    }

    async fn catalog_detail_by_isbn(
        &self,
        isbn: &str,
    ) -> bookclerk_source::Result<Option<CatalogHit>> {
        let (guest, _busy) = self.awake().await?;
        guest.catalog_detail_by_isbn(isbn).await
    }

    async fn expand_candidates(
        &self,
        seed: &ExpandSeed,
        limit: usize,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        let (guest, _busy) = self.awake().await?;
        guest.expand_candidates(seed, limit).await
    }

    async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
        let (guest, _busy) = self.awake().await?;
        guest.purchase_hint(opts).await
    }

    async fn purchase_hints(
        &self,
        opts: &[PurchaseHintOpts],
    ) -> bookclerk_source::Result<Vec<Option<SourcePurchaseHint>>> {
        let (guest, _busy) = self.awake().await?;
        guest.purchase_hints(opts).await
    }

    async fn list_deals(&self, limit: usize) -> bookclerk_source::Result<Vec<CatalogHit>> {
        let (guest, _busy) = self.awake().await?;
        guest.list_deals(limit).await
    }
}

#[cfg(test)]
#[path = "on_demand_source_tests.rs"]
mod tests;
