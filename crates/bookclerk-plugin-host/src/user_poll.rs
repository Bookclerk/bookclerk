//! The host-scheduled poll that asks a remote library for new users.
//!
//! A `remoteLibrary` guest queues users it has not reported yet (for
//! Audiobookshelf, accounts created on the server) and hands them over from
//! `poll_events`. The host owns the schedule: it calls the guest every
//! interval and passes each user to the daemon's `on_external_user` hook,
//! which mints a claim ticket. A resident guest is polled through its
//! session; an on-demand proxy polls through a wake, so a guest that sleeps
//! between polls still reports every user.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bookclerk_integrations::ExternalUser;
use tokio::sync::Notify;

/// How often the host polls a resident remote library for new users.
///
/// A resident guest is running anyway, so a short interval costs one RPC.
pub const RESIDENT_USER_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How often the host polls a sleeping remote library for new users.
///
/// Each poll wakes the guest, which then stays up for the idle timeout, so
/// the interval is several idle timeouts long: the guest sleeps most of the
/// time, and a new user waits at most this long for a claim ticket.
pub const ON_DEMAND_USER_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The daemon hook each polled user is handed to.
pub(crate) type OnExternalUser = Arc<dyn Fn(ExternalUser) + Send + Sync>;

/// Ends a poll loop when its owner starts again or stops.
///
/// Each start takes a new generation; a loop keeps running only while the
/// generation it was spawned with is still current.
#[derive(Default)]
pub(crate) struct PollGeneration {
    /// The current generation; bumped by every start and stop.
    current: Arc<AtomicU64>,
    /// Wakes the running loop for an immediate poll; `None` while no loop
    /// runs, so an ask with no loop leaves nothing behind.
    wake: std::sync::Mutex<Option<Arc<Notify>>>,
}

impl PollGeneration {
    /// Ends any running loop and returns the generation a new loop runs
    /// under.
    pub(crate) fn next(&self) -> u64 {
        self.current.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Ends any running loop.
    pub(crate) fn end(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
        if let Some(wake) = self.wake_slot().take() {
            // A loop waiting out its interval sees the new generation now.
            wake.notify_one();
        }
    }

    /// Asks the running loop to poll now instead of at the end of its
    /// interval (a guest said a pushed message announced a new user). Asks
    /// that arrive while a poll runs are folded into one more poll. Does
    /// nothing while no loop runs.
    pub(crate) fn poll_now(&self) {
        if let Some(wake) = self.wake_slot().as_ref() {
            wake.notify_one();
        }
    }

    /// The wake slot, recovered from a poisoned lock (it holds no
    /// invariant a panic could break).
    fn wake_slot(&self) -> std::sync::MutexGuard<'_, Option<Arc<Notify>>> {
        self.wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A fresh wake for a new loop, replacing the last loop's.
    fn new_wake(&self) -> Arc<Notify> {
        let wake = Arc::new(Notify::new());
        *self.wake_slot() = Some(Arc::clone(&wake));
        wake
    }

    /// A handle the loop checks to see whether `generation` is still
    /// current.
    fn watch(&self, generation: u64) -> impl Fn() -> bool + Send + 'static {
        let current = Arc::clone(&self.current);
        move || current.load(Ordering::SeqCst) == generation
    }
}

/// Spawns the poll loop for one remote library.
///
/// The loop waits `interval` (or less, when [`PollGeneration::poll_now`]
/// asks), calls `poll`, and hands every user it returns to `on_user`, until `generations` moves past the generation it was
/// spawned under or `poll` returns `None` (its owner is gone). A failed poll
/// is logged and the loop keeps its schedule.
///
/// # Arguments
///
/// * `generations` - the owner's generation counter; this loop runs under
///   a fresh generation from it.
/// * `interval` - time between polls; the first poll is one interval after
///   the start.
/// * `plugin_id` - alias for log lines and for users whose `provider` is
///   empty.
/// * `on_user` - the daemon hook each user is passed to.
/// * `poll` - one poll; `None` ends the loop.
pub(crate) fn spawn<F, Fut>(
    generations: &PollGeneration,
    interval: Duration,
    plugin_id: String,
    on_user: OnExternalUser,
    poll: F,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Option<bookclerk_integrations::Result<Vec<ExternalUser>>>> + Send,
{
    let current = generations.watch(generations.next());
    let wake = generations.new_wake();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = wake.notified() => {}
            }
            if !current() {
                break;
            }
            let Some(result) = poll().await else {
                break;
            };
            // A start or stop during the poll owns the schedule now; the
            // users it found still go to the hook, since the guest has
            // already handed them over.
            match result {
                Ok(users) => {
                    for user in users {
                        on_user(with_provider(user, &plugin_id));
                    }
                }
                Err(err) => {
                    tracing::warn!(id = %plugin_id, %err, "remote library user poll failed");
                }
            }
            if !current() {
                break;
            }
        }
    });
}

/// `user` with `plugin_id` as its provider when the guest left it empty.
fn with_provider(user: ExternalUser, plugin_id: &str) -> ExternalUser {
    if user.provider.is_empty() {
        ExternalUser {
            provider: plugin_id.to_string(),
            ..user
        }
    } else {
        user
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A user with the given provider.
    fn user(provider: &str) -> ExternalUser {
        ExternalUser {
            provider: provider.into(),
            external_user_id: "u1".into(),
            display_name: Some("ada".into()),
            access_token: None,
        }
    }

    /// A hook that records every user it receives.
    fn recorder() -> (OnExternalUser, Arc<std::sync::Mutex<Vec<ExternalUser>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let hook: OnExternalUser = Arc::new(move |user| sink.lock().expect("seen").push(user));
        (hook, seen)
    }

    #[test]
    fn an_empty_provider_becomes_the_plugin_alias() {
        assert_eq!(with_provider(user(""), "abs").provider, "abs");
        assert_eq!(with_provider(user("other"), "abs").provider, "other");
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_polls_each_interval_until_the_next_generation() {
        let generations = PollGeneration::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let (hook, seen) = recorder();
        let counter = Arc::clone(&polls);
        spawn(
            &generations,
            Duration::from_secs(10),
            "abs".into(),
            hook,
            move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    Some(if n == 1 {
                        Err(bookclerk_integrations::IntegrationError::message("down"))
                    } else {
                        Ok(vec![user("")])
                    })
                }
            },
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(polls.load(Ordering::SeqCst), 0, "the first poll waits");
        tokio::time::sleep(Duration::from_secs(26)).await;
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        // The failed second poll was logged; the others reached the hook.
        let users = seen.lock().expect("seen").clone();
        assert_eq!(users.len(), 2);
        assert!(users.iter().all(|u| u.provider == "abs"));
        generations.end();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(polls.load(Ordering::SeqCst), 3, "an ended loop stops");
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_ends_when_its_owner_is_gone() {
        let generations = PollGeneration::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let (hook, _seen) = recorder();
        let counter = Arc::clone(&polls);
        spawn(
            &generations,
            Duration::from_secs(1),
            "abs".into(),
            hook,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { None }
            },
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }
}
