//! On-demand activation for storage destinations.
//!
//! An [`OnDemandStorage`] stands in the destination registry for one
//! deployed first-party `s3` or `local` instance whether or not its guest is
//! running. Every storage call starts the guest; the shared
//! [`Activation`](crate::activation) core stops it after
//! [`STORAGE_IDLE_TIMEOUT`] without a call. A write returns once the object
//! is committed (S3 completes or aborts its multipart upload inside the
//! call), so the call in flight covers it. A streamed read hands back a byte
//! stream that keeps reading from the guest after the call returns, so the
//! stream carries its own hold and the guest stays up until it is dropped.
//! Jobs that run against the guest's session take a hold for as long as
//! they use it ([`OnDemandStorage::session`]).
//!
//! The namespace identity ([`StorageBackend::instance_id`]), scan placement
//! and copy support are captured from the first guest, so they stay the
//! same across wakes.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bookclerk_storage::{
    ByteRange, ListPage, ObjectInfo, ObjectMeta, ObjectProbe, PutStreamResult, StorageBackend,
    StorageError,
};
use bytes::Bytes;
use tokio::io::{AsyncRead, ReadBuf};

use crate::activation::{Activation, Busy, Guest, GuestStarter, OnDemandStatus, WakeError};
use crate::rpc_session::PluginSession;
use crate::{PluginFamily, PluginManifest};

/// How long a storage destination stays up after its last call returns.
///
/// Storage work comes in bursts (an acquire writes several parts and a
/// cover, a library scan pages through the inventory), so the timeout
/// matches storefronts.
pub const STORAGE_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// True when a storage deployment of `manifest` may sleep while idle.
///
/// The plugin must belong only to the storage family and must not be
/// Python, whose guests take seconds to start. The deployment also requires
/// a host-side backend (the first-party `s3` or `local` package); other
/// storage guests stay running.
///
/// # Arguments
///
/// * `manifest` - the installed manifest the deployment spawns.
#[must_use]
pub fn storage_activates_on_demand(manifest: &PluginManifest) -> bool {
    manifest.families() == [PluginFamily::Output] && !manifest.declares_python()
}

/// A running storage guest: its session and the backend that talks to it.
pub(crate) trait AwakeStorage: Guest {
    /// The backend that forwards storage calls to this guest.
    fn backend(&self) -> &dyn StorageBackend;

    /// The guest's session, for jobs that call its entrypoints directly.
    /// `None` for test doubles that have no session.
    fn session(&self) -> Option<Arc<PluginSession>>;
}

/// The production storage guest: a session and the [`StorageBackend`] over it.
pub(crate) struct StorageGuest {
    /// Guest session.
    pub(crate) session: Arc<PluginSession>,
    /// Backend over `session`.
    pub(crate) backend: Arc<dyn StorageBackend>,
}

impl Guest for StorageGuest {
    fn guest_running(&self) -> bool {
        self.session.guest_running()
    }

    fn guest_pid(&self) -> Option<u32> {
        self.session.guest_pid()
    }
}

impl AwakeStorage for StorageGuest {
    fn backend(&self) -> &dyn StorageBackend {
        self.backend.as_ref()
    }

    fn session(&self) -> Option<Arc<PluginSession>> {
        Some(Arc::clone(&self.session))
    }
}

/// The values a sleeping proxy answers without waking its guest, captured
/// from the guest the deployment first started.
struct Identity {
    /// Backend name for logs (`local`, `s3`).
    name: &'static str,
    /// Namespace identity; copies compare it, so it must not change per wake.
    instance_id: String,
    /// Host placement for scans.
    scan_placement: Option<String>,
    /// Whether copies stay inside the backend.
    supports_server_copy: bool,
    /// The intent operations the guest does natively.
    capabilities: bookclerk_storage::StorageCapabilities,
}

impl Identity {
    /// Reads every sleep-time answer from a running backend.
    fn of(backend: &dyn StorageBackend) -> Self {
        Self {
            name: backend.name(),
            instance_id: backend.instance_id(),
            scan_placement: backend.scan_placement(),
            supports_server_copy: backend.supports_server_copy(),
            capabilities: backend.capabilities(),
        }
    }
}

/// Destination registry entry that starts its storage guest on a call and
/// stops it after the idle timeout without calls.
///
/// Cloning shares the guest: every clone is the same destination.
#[derive(Clone)]
pub struct OnDemandStorage {
    /// Answers given while the guest sleeps.
    identity: Arc<Identity>,
    /// Deployment instance id, for errors.
    plugin_instance_id: Arc<str>,
    /// The running guest, started on a call and stopped when idle.
    activation: Arc<Activation<dyn AwakeStorage>>,
}

/// A guest session lent to a caller, with the hold that keeps an on-demand
/// guest up.
pub struct HeldSession {
    /// The running guest's session.
    pub session: Arc<PluginSession>,
    /// Keeps an on-demand guest from sleeping until dropped; `None` for a
    /// resident guest.
    _busy: Option<Busy>,
}

impl HeldSession {
    /// Lends a resident session, which needs no hold.
    pub(crate) fn resident(session: Arc<PluginSession>) -> Self {
        Self {
            session,
            _busy: None,
        }
    }
}

impl OnDemandStorage {
    /// Wraps `guest`, already started by the deployment, and begins the
    /// idle sweep.
    ///
    /// # Arguments
    ///
    /// * `guest` - the running storage guest; its backend's answers become the sleep-time identity.
    /// * `plugin_instance_id` - deployment instance the guest serves.
    /// * `starter` - spawns a replacement guest on each wake.
    /// * `idle_timeout` - idle time before the guest is stopped.
    ///
    /// # Returns
    ///
    /// The shared proxy to register in place of the guest.
    pub(crate) fn wrap(
        guest: Arc<dyn AwakeStorage>,
        plugin_instance_id: &str,
        starter: Box<dyn GuestStarter<dyn AwakeStorage>>,
        idle_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity: Arc::new(Identity::of(guest.backend())),
            plugin_instance_id: Arc::from(plugin_instance_id),
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
    async fn awake(&self) -> bookclerk_storage::Result<(Arc<dyn AwakeStorage>, Busy)> {
        self.activation.awake().await.map_err(|err| match err {
            WakeError::Retired => StorageError::Other(anyhow::anyhow!(
                "storage destination `{}` was stopped",
                self.plugin_instance_id
            )),
            WakeError::Start(detail) => StorageError::Other(anyhow::anyhow!(
                "storage destination `{}` could not start: {detail}",
                self.plugin_instance_id
            )),
        })
    }

    /// The running guest's session, starting the guest when asleep, held
    /// awake until the returned value is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy is retired, the guest cannot start, or
    /// the guest has no session (a test double).
    pub async fn session(&self) -> std::result::Result<HeldSession, String> {
        let (guest, busy) = self.awake().await.map_err(|err| err.to_string())?;
        let session = guest.session().ok_or_else(|| {
            format!(
                "storage destination `{}` has no session",
                self.plugin_instance_id
            )
        })?;
        Ok(HeldSession {
            session,
            _busy: Some(busy),
        })
    }
}

/// A read stream from a storage guest that keeps the guest awake until it
/// is dropped.
struct HeldReader {
    /// The guest's byte stream.
    inner: Pin<Box<dyn AsyncRead + Send>>,
    /// Keeps the guest from sleeping while the stream is open.
    _busy: Busy,
}

impl AsyncRead for HeldReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

#[async_trait]
impl OnDemandStatus for OnDemandStorage {
    fn is_retired(&self) -> bool {
        self.activation.is_retired()
    }

    async fn retire(&self) {
        // A storage guest has no stop RPC; dropping the session ends it, so
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
impl StorageBackend for OnDemandStorage {
    fn name(&self) -> &'static str {
        self.identity.name
    }

    fn instance_id(&self) -> String {
        self.identity.instance_id.clone()
    }

    fn scan_placement(&self) -> Option<String> {
        self.identity.scan_placement.clone()
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().put(key, data, meta).await
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().put_file(key, path, meta).await
    }

    async fn get(&self, key: &str) -> bookclerk_storage::Result<Bytes> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().get(key).await
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().exists(key).await
    }

    async fn list(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().list(prefix).await
    }

    async fn list_audio(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().list_audio(prefix).await
    }

    async fn probe(&self, key: &str) -> bookclerk_storage::Result<ObjectProbe> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().probe(key).await
    }

    async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().copy(from, to).await
    }

    async fn rename(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().rename(from, to).await
    }

    async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().delete(key).await
    }

    async fn head(&self, key: &str) -> bookclerk_storage::Result<Option<ObjectProbe>> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().head(key).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> bookclerk_storage::Result<ListPage> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().list_page(prefix, cursor, limit).await
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> bookclerk_storage::Result<(ObjectProbe, Pin<Box<dyn AsyncRead + Send>>)> {
        let (guest, busy) = self.awake().await?;
        let (probe, body) = guest.backend().get_stream(key, range).await?;
        let held: Pin<Box<dyn AsyncRead + Send>> = Box::pin(HeldReader {
            inner: body,
            _busy: busy,
        });
        Ok((probe, held))
    }

    async fn put_stream(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<PutStreamResult> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().put_stream(key, body, meta).await
    }

    fn supports_server_copy(&self) -> bool {
        self.identity.supports_server_copy
    }

    fn capabilities(&self) -> bookclerk_storage::StorageCapabilities {
        self.identity.capabilities
    }

    async fn move_object(
        &self,
        from: &str,
        to: &str,
        options: &bookclerk_storage::MoveOptions,
    ) -> bookclerk_storage::Result<bookclerk_storage::MoveResult> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().move_object(from, to, options).await
    }

    async fn move_many(
        &self,
        pairs: &[(String, String)],
        options: &bookclerk_storage::MoveOptions,
    ) -> bookclerk_storage::Result<Vec<bookclerk_storage::KeyResult>> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().move_many(pairs, options).await
    }

    async fn move_prefix(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        cursor: Option<&str>,
        limit: u32,
        options: &bookclerk_storage::MoveOptions,
    ) -> bookclerk_storage::Result<bookclerk_storage::MovePrefixPage> {
        let (guest, _busy) = self.awake().await?;
        guest
            .backend()
            .move_prefix(from_prefix, to_prefix, cursor, limit, options)
            .await
    }

    async fn delete_many(
        &self,
        keys: &[String],
    ) -> bookclerk_storage::Result<Vec<bookclerk_storage::KeyResult>> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().delete_many(keys).await
    }

    async fn head_many(
        &self,
        keys: &[String],
    ) -> bookclerk_storage::Result<Vec<Option<ObjectProbe>>> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().head_many(keys).await
    }

    async fn put_stream_conditional(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
        condition: &bookclerk_storage::WriteCondition,
    ) -> bookclerk_storage::Result<bookclerk_storage::ConditionalPutResult> {
        let (guest, _busy) = self.awake().await?;
        guest
            .backend()
            .put_stream_conditional(key, body, meta, condition)
            .await
    }

    async fn list_hinted(
        &self,
        request: &bookclerk_storage::ListRequest,
    ) -> bookclerk_storage::Result<bookclerk_storage::HintedListPage> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().list_hinted(request).await
    }

    async fn changes(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> bookclerk_storage::Result<Option<bookclerk_storage::ChangeFeedPage>> {
        if !self.identity.capabilities.changes {
            return Ok(None);
        }
        let (guest, _busy) = self.awake().await?;
        guest.backend().changes(cursor, limit).await
    }

    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> bookclerk_storage::Result<()> {
        let (guest, _busy) = self.awake().await?;
        guest.backend().touch_file(key, created, modified).await
    }
}

#[cfg(test)]
#[path = "on_demand_storage_tests.rs"]
mod tests;
