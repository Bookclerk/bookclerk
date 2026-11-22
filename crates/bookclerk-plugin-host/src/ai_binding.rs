//! The `AI` binding the host hands a plugin that calls models.
//!
//! A plugin receives it only when its manifest lists tasks under
//! `[ai].calls` and the covering grant carries the token for at least one
//! of them ([`crate::ai_task_grant`]). Each run is checked, limited to the
//! granted tasks, counted against the session's budget and bounded by a
//! deadline before the node's model router
//! ([`bookclerk_integrations::AiRouter`]) sees it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bookclerk_integrations::AiRouter;
use bookclerk_plugin_abi::ai::check_binding_run_params;
use bookclerk_plugin_abi::{AiInput, AiRunOk, AiRunParams, AiRunner, AiTask};
use bookclerk_plugin_sdk::PluginError as AbiError;

/// Input items (texts, for text embeddings) one plugin session may send
/// through `AI` per minute. The budget refills continuously and allows a
/// burst of the whole minute. Work over many items belongs in a job that
/// the host runs against the model plugin itself.
pub const AI_ITEMS_PER_MINUTE: u32 = 240;

/// Longest one run through the `AI` binding may take.
pub const AI_BINDING_RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// A budget of input items that refills at a steady rate.
#[derive(Debug)]
struct Budget {
    /// Items left, in thousandths of an item so refills stay exact.
    milli_items: u64,
    /// When `milli_items` was last brought up to date.
    at: Instant,
}

impl Budget {
    /// The full budget, in thousandths of an item.
    fn full() -> u64 {
        u64::from(AI_ITEMS_PER_MINUTE) * 1000
    }

    /// A full budget as of `now`.
    fn new(now: Instant) -> Self {
        Self {
            milli_items: Self::full(),
            at: now,
        }
    }

    /// Takes `items` from the budget.
    ///
    /// # Errors
    ///
    /// How long until the budget holds `items`, when it does not now.
    fn take(&mut self, items: u64, now: Instant) -> Result<(), Duration> {
        let elapsed_ms =
            u64::try_from(now.saturating_duration_since(self.at).as_millis()).unwrap_or(u64::MAX);
        // `AI_ITEMS_PER_MINUTE` items a minute is that many thousandths
        // every 60 ms.
        let refill = elapsed_ms.saturating_mul(u64::from(AI_ITEMS_PER_MINUTE)) / 60;
        self.milli_items = self.milli_items.saturating_add(refill).min(Self::full());
        self.at = now;
        let want = items.saturating_mul(1000);
        if want <= self.milli_items {
            self.milli_items -= want;
            return Ok(());
        }
        let short = want - self.milli_items;
        let wait_ms = short.saturating_mul(60) / u64::from(AI_ITEMS_PER_MINUTE);
        Err(Duration::from_millis(wait_ms.max(1)))
    }
}

/// How many input items a run carries.
fn input_items(input: &AiInput) -> u64 {
    u64::try_from(input.text_embeddings.texts.len()).unwrap_or(u64::MAX)
}

/// The `AI` binding of one plugin session.
pub(crate) struct GrantedAi {
    /// The node's model router.
    router: Arc<AiRouter>,
    /// Id of the calling plugin (`plugin.toml` `id`).
    caller: String,
    /// Tasks the manifest declares and the grant covers.
    tasks: Vec<AiTask>,
    /// The session's item budget.
    budget: std::sync::Mutex<Budget>,
}

impl GrantedAi {
    /// The binding for one session, when it may have one.
    ///
    /// # Arguments
    ///
    /// * `router` - The node's model router, when this host has one.
    /// * `manifest` - The plugin's manifest.
    /// * `grant` - The grant that covers this spawn.
    ///
    /// # Returns
    ///
    /// The binding when a router is present and at least one task the
    /// manifest lists under `[ai].calls` is granted; `None` otherwise, so
    /// the guest has no `AI`.
    pub(crate) fn granted(
        router: Option<Arc<AiRouter>>,
        manifest: &crate::PluginManifest,
        grant: &crate::PluginGrant,
    ) -> Option<Arc<Self>> {
        let tasks: Vec<AiTask> = manifest
            .ai
            .calls
            .iter()
            .copied()
            .filter(|task| grant.allows_ai_task(*task))
            .collect();
        if tasks.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            router: router?,
            caller: manifest.id.clone(),
            tasks,
            budget: std::sync::Mutex::new(Budget::new(Instant::now())),
        }))
    }
}

#[async_trait::async_trait(?Send)]
impl AiRunner for GrantedAi {
    async fn run(&self, params: AiRunParams) -> Result<AiRunOk, AbiError> {
        let (_, task) = check_binding_run_params(&params)?;
        if !self.tasks.contains(&task) {
            return Err(AbiError::forbidden(format!(
                "this plugin is not granted the `{}` task",
                task.wire_name()
            )));
        }
        let taken = self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(input_items(&params.input), Instant::now());
        if let Err(wait) = taken {
            return Err(AbiError::rate_limited(
                "this plugin's model budget is spent; work over many items belongs in a job",
                Some(wait),
            ));
        }
        self.router
            .run(Some(&self.caller), params, AI_BINDING_RUN_TIMEOUT)
            .await
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_allows_a_burst_then_refills_at_its_rate() {
        let start = Instant::now();
        let mut budget = Budget::new(start);
        assert_eq!(budget.take(u64::from(AI_ITEMS_PER_MINUTE), start), Ok(()));
        let wait = budget.take(4, start).unwrap_err();
        assert_eq!(wait, Duration::from_secs(1), "4 items at 240 a minute");
        assert!(budget.take(4, start + Duration::from_millis(900)).is_err());
        assert_eq!(budget.take(4, start + Duration::from_secs(1)), Ok(()));
        // A long idle never banks more than one minute.
        let later = start + Duration::from_secs(3600);
        assert_eq!(budget.take(u64::from(AI_ITEMS_PER_MINUTE), later), Ok(()));
        assert!(budget.take(1, later).is_err());
        // More than the whole budget can never be taken at once.
        let full = later + Duration::from_secs(3600);
        assert!(budget
            .take(u64::from(AI_ITEMS_PER_MINUTE) + 1, full)
            .is_err());
    }
}
