//! The start-on-call, stop-when-idle core shared by every on-demand proxy.
//!
//! An [`Activation`] owns at most one running guest of some kind `G` (an
//! integration, a storefront). A call takes a [`Busy`] guard and the running
//! guest from [`Activation::awake`], which starts a guest through its
//! [`GuestStarter`] when none is running. A sweep task stops the guest once
//! no call has been in flight for the idle timeout. The proxies in
//! [`crate::on_demand`] and [`crate::on_demand_source`] wrap one each and
//! forward their trait methods through it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// A running guest the activation forwards to, with the process facts it
/// needs.
#[async_trait]
pub(crate) trait Guest: Send + Sync {
    /// True while the guest process is still running.
    fn guest_running(&self) -> bool;

    /// Native guest pid, when there is one.
    fn guest_pid(&self) -> Option<u32>;

    /// Asks the guest to wind down before its last reference is dropped.
    ///
    /// The default does nothing: dropping the session stops the process.
    ///
    /// # Errors
    ///
    /// Returns the guest's stop failure as text; the guest is dropped anyway.
    async fn stop_guest(&self) -> std::result::Result<(), String> {
        Ok(())
    }
}

/// Starts a fresh guest of kind `G` for one deployment instance.
#[async_trait]
pub(crate) trait GuestStarter<G: ?Sized>: Send + Sync {
    /// Spawns a guest and returns it once it is ready for calls.
    ///
    /// # Errors
    ///
    /// Returns the spawn, `open` or health failure as text.
    async fn start_guest(&self) -> std::result::Result<Arc<G>, String>;
}

/// What the deployment reconciler reads from, and does to, any on-demand
/// proxy.
#[async_trait]
pub trait OnDemandStatus: Send + Sync {
    /// True once the deployment stopped the proxy; it does not wake again
    /// unless a rollback revives it.
    fn is_retired(&self) -> bool;

    /// Stops the proxy for good: a running guest stops and later calls
    /// fail, including calls through a registry snapshot a job still holds.
    async fn retire(&self);

    /// True while a guest is running. False while a wake or a sleep holds
    /// the guest slot.
    fn is_awake(&self) -> bool;

    /// Native pid of the running guest, if awake.
    fn guest_pid(&self) -> Option<u32>;

    /// Guests started on demand since the proxy was created.
    fn wakes(&self) -> u64;

    /// Idle time after which the guest is stopped.
    fn idle_timeout(&self) -> Duration;
}

/// Why [`Activation::awake`] returned no guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WakeError {
    /// The deployment stopped the proxy.
    Retired,
    /// The starter failed; carries its detail.
    Start(String),
}

/// Calls in flight and when the last one finished.
struct Activity {
    /// Calls that hold a [`Busy`] guard right now.
    in_flight: usize,
    /// When the guest last started or a call last finished.
    last_used: Instant,
}

/// Marks one call in flight until dropped, then records the finish time.
pub(crate) struct Busy<'a> {
    /// Activity record this guard counts in.
    activity: &'a std::sync::Mutex<Activity>,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut activity = lock_activity(self.activity);
        activity.in_flight = activity.in_flight.saturating_sub(1);
        activity.last_used = Instant::now();
    }
}

/// Locks an activity record.
///
/// A poisoned lock still yields the record: it holds a counter and a
/// timestamp that no panic can leave half-written.
fn lock_activity(activity: &std::sync::Mutex<Activity>) -> std::sync::MutexGuard<'_, Activity> {
    activity
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// At most one running guest, started on a call and stopped when idle.
pub(crate) struct Activation<G: ?Sized> {
    /// Deployment instance id, for logs.
    instance: String,
    /// Starts a fresh guest on wake.
    starter: Box<dyn GuestStarter<G>>,
    /// Idle time after which the guest is stopped.
    idle_timeout: Duration,
    /// The running guest, or `None` while asleep. Held across a wake so
    /// concurrent callers share one spawn.
    guest: tokio::sync::Mutex<Option<Arc<G>>>,
    /// In-flight calls and last use; the sweep reads it.
    activity: std::sync::Mutex<Activity>,
    /// Set by [`Activation::retire`]; a retired activation does not wake
    /// until [`Activation::revive`] clears it.
    retired: AtomicBool,
    /// Guests started on demand, for logs and tests.
    wakes: AtomicU64,
}

impl<G: ?Sized + Guest + 'static> Activation<G> {
    /// An activation that starts out with `guest` running and sweeps itself
    /// for idleness until it is dropped.
    ///
    /// # Arguments
    ///
    /// * `guest` - the guest the deployment already started.
    /// * `instance` - deployment instance id, for logs.
    /// * `starter` - spawns a replacement guest on each wake.
    /// * `idle_timeout` - idle time before the guest is stopped.
    pub(crate) fn start(
        guest: Arc<G>,
        instance: &str,
        starter: Box<dyn GuestStarter<G>>,
        idle_timeout: Duration,
    ) -> Arc<Self> {
        let activation = Arc::new(Self {
            instance: instance.to_string(),
            starter,
            idle_timeout,
            guest: tokio::sync::Mutex::new(Some(guest)),
            activity: std::sync::Mutex::new(Activity {
                in_flight: 0,
                last_used: Instant::now(),
            }),
            retired: AtomicBool::new(false),
            wakes: AtomicU64::new(0),
        });
        spawn_idle_sweep(Arc::downgrade(&activation), idle_timeout);
        activation
    }

    /// Idle time after which the guest is stopped.
    pub(crate) fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// True once [`Self::retire`] ran.
    pub(crate) fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// True while a guest is running and no wake or sleep holds the slot.
    pub(crate) fn is_awake(&self) -> bool {
        self.guest
            .try_lock()
            .is_ok_and(|guest| guest.as_ref().is_some_and(|guest| guest.guest_running()))
    }

    /// Native pid of the running guest, if awake.
    pub(crate) fn guest_pid(&self) -> Option<u32> {
        self.guest
            .try_lock()
            .ok()
            .and_then(|guest| guest.as_ref().and_then(|guest| guest.guest_pid()))
    }

    /// Guests started on demand since the activation was created.
    pub(crate) fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::SeqCst)
    }

    /// The running guest, without waking a sleeping one.
    pub(crate) async fn running(&self) -> Option<Arc<G>> {
        self.guest
            .lock()
            .await
            .as_ref()
            .filter(|guest| guest.guest_running())
            .cloned()
    }

    /// Marks a call in flight, then returns the running guest, starting one
    /// when asleep or when the last guest exited.
    ///
    /// # Errors
    ///
    /// [`WakeError::Retired`] after [`Self::retire`]; [`WakeError::Start`]
    /// when the starter fails.
    pub(crate) async fn awake(&self) -> std::result::Result<(Arc<G>, Busy<'_>), WakeError> {
        // Counted before taking the slot so a sweep that wins the slot
        // first sees the call and leaves the guest running.
        lock_activity(&self.activity).in_flight += 1;
        let busy = Busy {
            activity: &self.activity,
        };
        if self.is_retired() {
            return Err(WakeError::Retired);
        }
        let mut slot = self.guest.lock().await;
        // `retire` may have run while this call waited for the slot; a
        // retired activation must not start a guest.
        if self.is_retired() {
            return Err(WakeError::Retired);
        }
        if let Some(guest) = slot.as_ref() {
            if guest.guest_running() {
                return Ok((Arc::clone(guest), busy));
            }
        }
        if let Some(exited) = slot.take() {
            let _ = exited.stop_guest().await;
        }
        let started = Instant::now();
        let guest = self.starter.start_guest().await.map_err(WakeError::Start)?;
        // A retirement that landed while the guest started wins: stop the
        // new guest rather than hand it work.
        if self.is_retired() {
            let _ = guest.stop_guest().await;
            return Err(WakeError::Retired);
        }
        self.wakes.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(
            plugin_instance_id = %self.instance,
            elapsed_ms = started.elapsed().as_millis(),
            "plugin started on demand"
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
    pub(crate) async fn sleep_if_idle(&self) -> bool {
        let Ok(mut slot) = self.guest.try_lock() else {
            return false;
        };
        {
            let activity = lock_activity(&self.activity);
            if activity.in_flight > 0 || activity.last_used.elapsed() < self.idle_timeout {
                return false;
            }
        }
        let Some(guest) = slot.take() else {
            return false;
        };
        drop(slot);
        if let Err(err) = guest.stop_guest().await {
            tracing::warn!(
                plugin_instance_id = %self.instance,
                error = %err,
                "idle plugin stop failed"
            );
        }
        tracing::debug!(plugin_instance_id = %self.instance, "idle plugin stopped");
        true
    }

    /// Stops the guest for good; later calls fail with
    /// [`WakeError::Retired`].
    ///
    /// # Errors
    ///
    /// Returns the running guest's stop failure as text.
    pub(crate) async fn retire(&self) -> std::result::Result<(), String> {
        self.retired.store(true, Ordering::SeqCst);
        let guest = self.guest.lock().await.take();
        match guest {
            Some(guest) => guest.stop_guest().await,
            None => Ok(()),
        }
    }

    /// Lets a retired activation wake again; the next call starts a fresh
    /// guest.
    ///
    /// A deployment whose replacement failed to start registers the previous
    /// proxy again and revives it through this.
    pub(crate) fn revive(&self) {
        self.retired.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
impl<G: ?Sized> Activation<G> {
    /// Calls holding a [`Busy`] guard right now.
    pub(crate) fn in_flight(&self) -> usize {
        lock_activity(&self.activity).in_flight
    }

    /// The guest slot, so a test can hold it the way a wake or `retire`
    /// does.
    pub(crate) fn slot(&self) -> &tokio::sync::Mutex<Option<Arc<G>>> {
        &self.guest
    }

    /// Marks the activation retired without taking the slot, as `retire`
    /// does before it waits for the slot.
    pub(crate) fn mark_retired(&self) {
        self.retired.store(true, Ordering::SeqCst);
    }
}

/// Checks `activation` for idleness a few times per timeout until it is
/// dropped.
///
/// A retired activation is skipped rather than abandoned, so one revived by
/// a rollback goes back to sleep when idle.
fn spawn_idle_sweep<G: ?Sized + Guest + 'static>(
    activation: Weak<Activation<G>>,
    idle_timeout: Duration,
) {
    let period = (idle_timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(15));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(activation) = activation.upgrade() else {
                return;
            };
            if activation.is_retired() {
                continue;
            }
            activation.sleep_if_idle().await;
        }
    });
}
