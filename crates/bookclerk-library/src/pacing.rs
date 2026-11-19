//! Download pacing per store account: the limits, the operator's override,
//! and the decision whether a download may start now.
//!
//! A storefront declares a pace in `plugin.toml` `[acquire]` (a daily
//! budget over any rolling 24 hours, a minimum gap between downloads, and
//! the cool-down to apply when the store answers `rateLimited` without a
//! retry time). The operator may override it per account. Downloads the
//! host starts itself wait for the pace ([`decide`]); downloads a person
//! starts by hand are counted but never held. This module is pure: the
//! ledger and the override live in the library database
//! (`LibraryStore::account_pace_state`), and `bookclerkd` holds a paced
//! acquire job between titles when [`decide`] says to wait.
//!
//! See `docs/internals/jobs.md#download-pacing`.

use std::num::NonZeroU32;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The window the daily budget counts downloads over.
pub const BUDGET_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a download stays in the ledger; older rows are pruned. Longer
/// than [`BUDGET_WINDOW`] so a clock step never empties the window early.
pub const LEDGER_RETENTION: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Cool-down when the store says to slow down without a retry time and the
/// storefront declares no `cooldown_seconds`.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60 * 60);

/// Longest cool-down the host records, whatever the store asked for: a
/// store answer that names a later time is capped here, so a bad
/// `retryAfterMs` cannot stop an account for good.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Most downloads a day an override may set (the manifest's cap too).
pub const MAX_DOWNLOADS_PER_DAY: u32 = 10_000;

/// Longest gap an override may set, in seconds (one day).
pub const MAX_MIN_GAP_SECONDS: u32 = 86_400;

/// A storefront's declared pace (`[acquire]`), or the limits in force after
/// an override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquirePace {
    /// Downloads allowed in any rolling [`BUDGET_WINDOW`]; `None` for no
    /// daily budget.
    pub downloads_per_day: Option<NonZeroU32>,
    /// Least time between the start of two downloads on one account.
    pub min_gap: Duration,
    /// Cool-down when the store says to slow down without a retry time.
    pub cooldown: Duration,
}

impl Default for AcquirePace {
    /// No budget, no gap, and the [`DEFAULT_COOLDOWN`]: the pace of a
    /// storefront that declares no `[acquire]` table.
    fn default() -> Self {
        Self {
            downloads_per_day: None,
            min_gap: Duration::ZERO,
            cooldown: DEFAULT_COOLDOWN,
        }
    }
}

impl AcquirePace {
    /// Builds a pace from a manifest's `[acquire]` values.
    ///
    /// Values the manifest validation already bounds are taken as they are;
    /// a value out of range here (a manifest built without validation) is
    /// clamped rather than refused, and a non-positive budget means none.
    ///
    /// # Arguments
    ///
    /// * `downloads_per_day` - `[acquire].downloads_per_day`.
    /// * `min_gap_seconds` - `[acquire].min_gap_seconds`.
    /// * `cooldown_seconds` - `[acquire].cooldown_seconds`.
    #[must_use]
    pub fn from_declared(
        downloads_per_day: Option<i64>,
        min_gap_seconds: Option<i64>,
        cooldown_seconds: Option<i64>,
    ) -> Self {
        let budget = downloads_per_day
            .and_then(|n| u32::try_from(n.min(i64::from(MAX_DOWNLOADS_PER_DAY))).ok())
            .and_then(NonZeroU32::new);
        let gap = min_gap_seconds
            .and_then(|s| u64::try_from(s.min(i64::from(MAX_MIN_GAP_SECONDS))).ok())
            .map_or(Duration::ZERO, Duration::from_secs);
        let cooldown = cooldown_seconds
            .and_then(|s| u64::try_from(s).ok())
            .filter(|s| *s > 0)
            .map_or(DEFAULT_COOLDOWN, |s| {
                Duration::from_secs(s).min(MAX_COOLDOWN)
            });
        Self {
            downloads_per_day: budget,
            min_gap: gap,
            cooldown,
        }
    }

    /// The limits in force once `override_` applies to this declared pace.
    ///
    /// [`PaceOverride::Unlimited`] lifts the budget and the gap but keeps
    /// the cool-down: a store that asked to slow down is still obeyed by
    /// automatic downloads until the operator clears the cool-down.
    #[must_use]
    pub fn with_override(self, override_: &PaceOverride) -> Self {
        match override_ {
            PaceOverride::Default => self,
            PaceOverride::Unlimited => Self {
                downloads_per_day: None,
                min_gap: Duration::ZERO,
                cooldown: self.cooldown,
            },
            PaceOverride::Custom {
                downloads_per_day,
                min_gap_seconds,
            } => Self {
                downloads_per_day: downloads_per_day.or(self.downloads_per_day),
                min_gap: min_gap_seconds
                    .map_or(self.min_gap, |s| Duration::from_secs(u64::from(s))),
                cooldown: self.cooldown,
            },
        }
    }
}

/// The operator's override of a storefront's pace for one account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PaceOverride {
    /// The storefront's declared pace.
    #[default]
    Default,
    /// Custom values; a field left `None` keeps the storefront's value.
    Custom {
        /// Downloads allowed in any rolling 24 hours.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        downloads_per_day: Option<NonZeroU32>,
        /// Least seconds between two downloads, at most
        /// [`MAX_MIN_GAP_SECONDS`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_gap_seconds: Option<u32>,
    },
    /// No budget and no gap (the cool-down still applies).
    Unlimited,
}

/// Why an override was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PaceOverrideError {
    /// The daily budget is above [`MAX_DOWNLOADS_PER_DAY`].
    #[error("downloads_per_day must be between 1 and {MAX_DOWNLOADS_PER_DAY}")]
    DownloadsPerDay,
    /// The gap is above [`MAX_MIN_GAP_SECONDS`].
    #[error("min_gap_seconds must be between 0 and {MAX_MIN_GAP_SECONDS}")]
    MinGap,
    /// A custom override with neither field set.
    #[error("a custom pace needs downloads_per_day or min_gap_seconds")]
    EmptyCustom,
}

impl PaceOverride {
    /// The stored `pace_mode` spelling (`default`, `custom`, `unlimited`).
    #[must_use]
    pub fn mode_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Custom { .. } => "custom",
            Self::Unlimited => "unlimited",
        }
    }

    /// The modes [`Self::mode_str`] returns, for the schema's CHECK list.
    pub const MODES: [&'static str; 3] = ["default", "custom", "unlimited"];

    /// Checks an override's values against their ranges.
    ///
    /// # Errors
    ///
    /// Returns a [`PaceOverrideError`] naming the first value out of range,
    /// or [`PaceOverrideError::EmptyCustom`] for a custom override that sets
    /// nothing.
    pub fn validate(&self) -> Result<(), PaceOverrideError> {
        if let Self::Custom {
            downloads_per_day,
            min_gap_seconds,
        } = self
        {
            if downloads_per_day.is_none() && min_gap_seconds.is_none() {
                return Err(PaceOverrideError::EmptyCustom);
            }
            if downloads_per_day.is_some_and(|n| n.get() > MAX_DOWNLOADS_PER_DAY) {
                return Err(PaceOverrideError::DownloadsPerDay);
            }
            if min_gap_seconds.is_some_and(|s| s > MAX_MIN_GAP_SECONDS) {
                return Err(PaceOverrideError::MinGap);
            }
        }
        Ok(())
    }

    /// Rebuilds an override from its stored row.
    ///
    /// An unknown mode or a custom row with any value out of range (a row
    /// written by something other than this crate) falls back to
    /// [`Self::Default`], so a bad row never lifts the storefront's pace;
    /// a value that is absent leaves that limit unset.
    #[must_use]
    pub fn from_row(
        mode: &str,
        downloads_per_day: Option<i64>,
        min_gap_seconds: Option<i64>,
    ) -> Self {
        let parsed = match mode {
            "unlimited" => Self::Unlimited,
            "custom" => {
                // A present value that does not convert is a bad row, not an
                // absent field: it must not leave the other limit alone.
                let Ok(downloads_per_day) = downloads_per_day
                    .map(|n| u32::try_from(n).ok().and_then(NonZeroU32::new).ok_or(()))
                    .transpose()
                else {
                    return Self::Default;
                };
                let Ok(min_gap_seconds) = min_gap_seconds
                    .map(|s| u32::try_from(s).map_err(|_| ()))
                    .transpose()
                else {
                    return Self::Default;
                };
                Self::Custom {
                    downloads_per_day,
                    min_gap_seconds,
                }
            }
            _ => Self::Default,
        };
        if parsed.validate().is_ok() {
            parsed
        } else {
            Self::Default
        }
    }
}

/// A change to one account's pace: the body of
/// `POST /api/portal/sources/{source}/connections/{account_id}/pace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaceRequest {
    /// The override to store, or `None` for a request that only clears
    /// the cool-down and leaves the stored pace as it is.
    pub override_: Option<PaceOverride>,
    /// End the store's cool-down too.
    pub clear_cooldown: bool,
}

/// The JSON shape of a [`PaceRequest`] before its rules are checked; read
/// only from a JSON object, never from an array ([`ObjectBody`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaceBody {
    /// `default`, `custom` or `unlimited`; absent to only clear the
    /// cool-down.
    #[serde(default)]
    mode: Option<PaceMode>,
    /// Custom downloads per rolling 24 hours.
    #[serde(default)]
    downloads_per_day: Option<u32>,
    /// Custom seconds between downloads.
    #[serde(default)]
    min_gap_seconds: Option<u32>,
    /// End the store's cool-down.
    #[serde(default)]
    clear_cooldown: Option<bool>,
}

/// The `mode` of a [`PaceBody`].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PaceMode {
    /// The storefront's declared pace.
    Default,
    /// Custom values.
    Custom,
    /// No budget and no gap.
    Unlimited,
}

/// A [`PaceBody`] read from a JSON object only: serde's derived struct
/// reader would also take a positional array.
struct ObjectBody(PaceBody);

impl<'de> Deserialize<'de> for ObjectBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::control_plane::from_object(deserializer, "a JSON object").map(Self)
    }
}

/// Parses and checks a pace change request (sans-IO, so the fuzz target and
/// the route run the same code).
///
/// The body is one JSON object with a `mode` of `default`, `custom` or
/// `unlimited`, optional `downloads_per_day` (1 to
/// [`MAX_DOWNLOADS_PER_DAY`]) and `min_gap_seconds` (0 to
/// [`MAX_MIN_GAP_SECONDS`]) that only `custom` may carry and that it needs
/// at least one of, and an optional boolean `clear_cooldown`. A body
/// without `mode` only clears the cool-down: it carries no number and
/// `clear_cooldown` must be `true`, and the stored pace is left alone. No
/// other key and no repeated key is accepted; `null` counts as absent.
///
/// # Errors
///
/// Returns [`crate::LibraryError::Invalid`] describing the first rule the
/// body breaks.
pub fn parse_pace_request(body: &[u8]) -> crate::error::Result<PaceRequest> {
    let invalid = |detail: String| crate::LibraryError::Invalid(format!("pace body: {detail}"));
    let ObjectBody(raw) =
        serde_json::from_slice::<ObjectBody>(body).map_err(|err| invalid(err.to_string()))?;
    let clear_cooldown = raw.clear_cooldown.unwrap_or(false);
    let Some(mode) = raw.mode else {
        if raw.downloads_per_day.is_some() || raw.min_gap_seconds.is_some() {
            return Err(invalid(
                "only a custom pace takes downloads_per_day or min_gap_seconds".into(),
            ));
        }
        if !clear_cooldown {
            return Err(invalid(
                "a body without mode must set clear_cooldown to true".into(),
            ));
        }
        return Ok(PaceRequest {
            override_: None,
            clear_cooldown,
        });
    };
    let override_ = match mode {
        PaceMode::Default | PaceMode::Unlimited
            if raw.downloads_per_day.is_some() || raw.min_gap_seconds.is_some() =>
        {
            return Err(invalid(
                "only a custom pace takes downloads_per_day or min_gap_seconds".into(),
            ));
        }
        PaceMode::Default => PaceOverride::Default,
        PaceMode::Unlimited => PaceOverride::Unlimited,
        PaceMode::Custom => {
            if raw.downloads_per_day == Some(0) {
                return Err(invalid(PaceOverrideError::DownloadsPerDay.to_string()));
            }
            PaceOverride::Custom {
                downloads_per_day: raw.downloads_per_day.and_then(NonZeroU32::new),
                min_gap_seconds: raw.min_gap_seconds,
            }
        }
    };
    override_
        .validate()
        .map_err(|err| invalid(err.to_string()))?;
    Ok(PaceRequest {
        override_: Some(override_),
        clear_cooldown,
    })
}

/// What the ledger says about one account's recent downloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadWindow {
    /// Start times of the downloads in the last [`BUDGET_WINDOW`], oldest
    /// first.
    pub starts: Vec<DateTime<Utc>>,
}

impl DownloadWindow {
    /// The latest download start, if any.
    #[must_use]
    pub fn last(&self) -> Option<DateTime<Utc>> {
        self.starts.last().copied()
    }

    /// Downloads in the window.
    #[must_use]
    pub fn count(&self) -> usize {
        self.starts.len()
    }
}

/// Why a paced download has to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// The store asked to slow down.
    Cooldown,
    /// The rolling 24-hour budget is used up.
    DailyBudget,
    /// The previous download started less than the gap ago.
    Gap,
}

impl WaitReason {
    /// A short phrase for job progress and logs ("the daily download
    /// budget").
    #[must_use]
    pub fn phrase(self) -> &'static str {
        match self {
            Self::Cooldown => "the store's cool-down",
            Self::DailyBudget => "the daily download budget",
            Self::Gap => "the gap between downloads",
        }
    }
}

/// Whether a paced download may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceVerdict {
    /// Start it now.
    Go,
    /// Wait until `until`.
    Wait {
        /// When the download may start.
        until: DateTime<Utc>,
        /// What it is waiting for (the limit that ends last).
        reason: WaitReason,
    },
}

/// Decides whether a paced download on one account may start at `now`.
///
/// Every limit that holds is checked and the one that ends last wins, so a
/// job parked until `until` finds every limit clear when it wakes (unless
/// another download started meanwhile).
///
/// # Arguments
///
/// * `pace` - The limits in force ([`AcquirePace::with_override`]).
/// * `window` - The account's downloads in the last [`BUDGET_WINDOW`].
/// * `cooldown_until` - The end of the store's cool-down, if one is set.
/// * `now` - The current time.
#[must_use]
pub fn decide(
    pace: &AcquirePace,
    window: &DownloadWindow,
    cooldown_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> PaceVerdict {
    let mut wait: Option<(DateTime<Utc>, WaitReason)> = None;
    let mut consider = |until: DateTime<Utc>, reason: WaitReason| {
        if until > now && wait.is_none_or(|(at, _)| until > at) {
            wait = Some((until, reason));
        }
    };
    if let Some(until) = cooldown_until {
        consider(until, WaitReason::Cooldown);
    }
    if let Some(budget) = pace.downloads_per_day {
        let budget = usize::try_from(budget.get()).unwrap_or(usize::MAX);
        let window_len = chrono::Duration::from_std(BUDGET_WINDOW).unwrap_or(chrono::Duration::MAX);
        let recent: Vec<DateTime<Utc>> = window
            .starts
            .iter()
            .copied()
            .filter(|at| *at + window_len > now)
            .collect();
        if recent.len() >= budget {
            // The slot frees when enough of the oldest downloads leave the
            // window that one more fits.
            if let Some(oldest) = recent.get(recent.len() - budget) {
                consider(*oldest + window_len, WaitReason::DailyBudget);
            }
        }
    }
    if !pace.min_gap.is_zero() {
        if let (Some(last), Ok(gap)) = (window.last(), chrono::Duration::from_std(pace.min_gap)) {
            consider(last + gap, WaitReason::Gap);
        }
    }
    match wait {
        Some((until, reason)) => PaceVerdict::Wait { until, reason },
        None => PaceVerdict::Go,
    }
}

/// The cool-down end to record after the store answered `rateLimited`.
///
/// The store's `retry_after` wins when it gave one; otherwise the
/// storefront's declared cool-down. Either is capped at [`MAX_COOLDOWN`].
///
/// # Arguments
///
/// * `pace` - The account's declared pace (for its cool-down).
/// * `retry_after` - How long the store asked to be left alone, if it said.
/// * `now` - The current time.
#[must_use]
pub fn cooldown_end(
    pace: &AcquirePace,
    retry_after: Option<Duration>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let wait = retry_after
        .filter(|d| !d.is_zero())
        .unwrap_or(pace.cooldown)
        .min(MAX_COOLDOWN);
    now + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("in range")
    }

    fn pace(per_day: u32, gap: u64) -> AcquirePace {
        AcquirePace {
            downloads_per_day: NonZeroU32::new(per_day),
            min_gap: Duration::from_secs(gap),
            cooldown: DEFAULT_COOLDOWN,
        }
    }

    #[test]
    fn an_empty_window_goes() {
        assert_eq!(
            decide(&pace(2, 30), &DownloadWindow::default(), None, at(0)),
            PaceVerdict::Go
        );
    }

    #[test]
    fn the_gap_holds_until_it_has_passed() {
        let window = DownloadWindow {
            starts: vec![at(0)],
        };
        assert_eq!(
            decide(&pace(10, 30), &window, None, at(10)),
            PaceVerdict::Wait {
                until: at(30),
                reason: WaitReason::Gap
            }
        );
        assert_eq!(
            decide(&pace(10, 30), &window, None, at(30)),
            PaceVerdict::Go
        );
    }

    #[test]
    fn a_full_budget_waits_for_the_oldest_download_to_age_out() {
        let window = DownloadWindow {
            starts: vec![at(0), at(100), at(200)],
        };
        // Budget 2, three downloads in the window: the second-oldest must
        // leave before one more fits.
        assert_eq!(
            decide(&pace(2, 0), &window, None, at(300)),
            PaceVerdict::Wait {
                until: at(100 + 86_400),
                reason: WaitReason::DailyBudget
            }
        );
        assert_eq!(
            decide(&pace(2, 0), &window, None, at(100 + 86_400)),
            PaceVerdict::Go
        );
    }

    #[test]
    fn downloads_older_than_the_window_do_not_count() {
        let window = DownloadWindow {
            starts: vec![at(0)],
        };
        assert_eq!(
            decide(&pace(1, 0), &window, None, at(86_400)),
            PaceVerdict::Go
        );
    }

    #[test]
    fn the_latest_limit_wins() {
        let window = DownloadWindow {
            starts: vec![at(0)],
        };
        assert_eq!(
            decide(&pace(10, 30), &window, Some(at(500)), at(10)),
            PaceVerdict::Wait {
                until: at(500),
                reason: WaitReason::Cooldown
            }
        );
        assert_eq!(
            decide(&pace(1, 30), &window, Some(at(500)), at(10)),
            PaceVerdict::Wait {
                until: at(86_400),
                reason: WaitReason::DailyBudget
            }
        );
    }

    #[test]
    fn a_past_cooldown_is_ignored() {
        assert_eq!(
            decide(&pace(10, 0), &DownloadWindow::default(), Some(at(0)), at(5)),
            PaceVerdict::Go
        );
    }

    #[test]
    fn unlimited_keeps_only_the_cooldown() {
        let declared = pace(1, 60);
        let open = declared.with_override(&PaceOverride::Unlimited);
        let window = DownloadWindow {
            starts: vec![at(0), at(1)],
        };
        assert_eq!(decide(&open, &window, None, at(2)), PaceVerdict::Go);
        assert!(matches!(
            decide(&open, &window, Some(at(100)), at(2)),
            PaceVerdict::Wait {
                reason: WaitReason::Cooldown,
                ..
            }
        ));
    }

    #[test]
    fn a_custom_override_keeps_unset_fields() {
        let declared = pace(100, 30);
        let custom = declared.with_override(&PaceOverride::Custom {
            downloads_per_day: NonZeroU32::new(5),
            min_gap_seconds: None,
        });
        assert_eq!(custom.downloads_per_day, NonZeroU32::new(5));
        assert_eq!(custom.min_gap, Duration::from_secs(30));
    }

    #[test]
    fn declared_values_are_clamped_and_defaulted() {
        let p = AcquirePace::from_declared(Some(0), Some(-5), None);
        assert_eq!(p, AcquirePace::default());
        let p = AcquirePace::from_declared(Some(50_000), Some(999_999), Some(99_999_999));
        assert_eq!(p.downloads_per_day, NonZeroU32::new(MAX_DOWNLOADS_PER_DAY));
        assert_eq!(
            p.min_gap,
            Duration::from_secs(u64::from(MAX_MIN_GAP_SECONDS))
        );
        assert_eq!(p.cooldown, MAX_COOLDOWN);
    }

    #[test]
    fn a_bad_stored_row_falls_back_to_default() {
        assert_eq!(
            PaceOverride::from_row("weird", None, None),
            PaceOverride::Default
        );
        assert_eq!(
            PaceOverride::from_row("custom", None, None),
            PaceOverride::Default
        );
        assert_eq!(
            PaceOverride::from_row("custom", Some(20_000), None),
            PaceOverride::Default
        );
        // A present value that does not convert spoils the whole row, even
        // when the other value is valid on its own.
        assert_eq!(
            PaceOverride::from_row("custom", Some(-1), Some(0)),
            PaceOverride::Default
        );
        assert_eq!(
            PaceOverride::from_row("custom", Some(0), Some(30)),
            PaceOverride::Default
        );
        assert_eq!(
            PaceOverride::from_row("custom", Some(5), Some(-3)),
            PaceOverride::Default
        );
        assert_eq!(
            PaceOverride::from_row("custom", None, Some(0)),
            PaceOverride::Custom {
                downloads_per_day: None,
                min_gap_seconds: Some(0)
            }
        );
        assert_eq!(
            PaceOverride::from_row("custom", Some(5), Some(10)),
            PaceOverride::Custom {
                downloads_per_day: NonZeroU32::new(5),
                min_gap_seconds: Some(10)
            }
        );
        assert_eq!(
            PaceOverride::from_row("unlimited", None, None),
            PaceOverride::Unlimited
        );
    }

    #[test]
    fn the_store_retry_time_wins_and_is_capped() {
        let p = pace(1, 0);
        assert_eq!(
            cooldown_end(&p, Some(Duration::from_secs(90)), at(0)),
            at(90)
        );
        assert_eq!(cooldown_end(&p, None, at(0)), at(3_600));
        assert_eq!(
            cooldown_end(&p, Some(Duration::from_secs(30 * 86_400)), at(0)),
            at(7 * 86_400)
        );
    }

    #[test]
    fn pace_requests_follow_their_rules() {
        let ok = parse_pace_request(
            br#"{"mode":"custom","downloads_per_day":20,"clear_cooldown":true}"#,
        )
        .expect("valid");
        assert_eq!(
            ok,
            PaceRequest {
                override_: Some(PaceOverride::Custom {
                    downloads_per_day: NonZeroU32::new(20),
                    min_gap_seconds: None
                }),
                clear_cooldown: true
            }
        );
        assert_eq!(
            parse_pace_request(br#"{"mode":"unlimited","min_gap_seconds":null}"#)
                .expect("null is absent")
                .override_,
            Some(PaceOverride::Unlimited)
        );
        // A body without mode only clears the cool-down.
        assert_eq!(
            parse_pace_request(br#"{"clear_cooldown":true}"#).expect("clear only"),
            PaceRequest {
                override_: None,
                clear_cooldown: true
            }
        );
        for bad in [
            &br#"{"mode":"custom"}"#[..],
            br#"{"mode":"custom","downloads_per_day":0}"#,
            br#"{"mode":"custom","downloads_per_day":10001}"#,
            br#"{"mode":"custom","min_gap_seconds":86401}"#,
            br#"{"mode":"default","downloads_per_day":5}"#,
            br#"{"mode":"fast"}"#,
            br#"{"mode":"default","extra":1}"#,
            br#"{"mode":"default","mode":"unlimited"}"#,
            br#"{}"#,
            br#"{"clear_cooldown":false}"#,
            br#"{"clear_cooldown":true,"min_gap_seconds":5}"#,
            br#"[]"#,
        ] {
            assert!(
                matches!(
                    parse_pace_request(bad),
                    Err(crate::LibraryError::Invalid(_))
                ),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn overrides_round_trip_as_tagged_json() {
        let custom = PaceOverride::Custom {
            downloads_per_day: NonZeroU32::new(20),
            min_gap_seconds: Some(60),
        };
        let json = serde_json::to_string(&custom).expect("serializes");
        assert_eq!(
            json,
            r#"{"mode":"custom","downloads_per_day":20,"min_gap_seconds":60}"#
        );
        let back: PaceOverride = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, custom);
    }
}
